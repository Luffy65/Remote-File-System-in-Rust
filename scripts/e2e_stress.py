#!/usr/bin/env python3
"""Cross-platform end-to-end stress and recovery tests for remote-fs.

The normal suite can run with any supported client/server OS pairing.  The
prepare-recovery/verify-recovery phases currently target the Windows durable
write journal and deliberately require two separate client processes.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
import random
import shutil
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from typing import BinaryIO, Iterable


CHUNK_SIZE = 4 * 1024 * 1024


class TestFailure(RuntimeError):
    pass


class ApiClient:
    def __init__(self, base_url: str, token: str | None, timeout: float) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout

    @staticmethod
    def encode_path(path: str) -> str:
        return "/".join(
            urllib.parse.quote(component, safe="")
            for component in path.strip("/").split("/")
            if component
        )

    def request(self, method: str, endpoint: str, path: str = "") -> BinaryIO:
        encoded = self.encode_path(path)
        url = f"{self.base_url}/{endpoint}/{encoded}"
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(url, method=method, headers=headers)
        return urllib.request.urlopen(request, timeout=self.timeout)

    def metadata(self, path: str) -> dict:
        with self.request("GET", "metadata", path) as response:
            return json.load(response)

    def list(self, path: str) -> list[dict]:
        with self.request("GET", "list", path) as response:
            return json.load(response)

    def hash_file(self, path: str) -> tuple[str, int]:
        digest = hashlib.sha256()
        size = 0
        with self.request("GET", "files", path) as response:
            while chunk := response.read(CHUNK_SIZE):
                digest.update(chunk)
                size += len(chunk)
        return digest.hexdigest(), size

    def delete_tree(self, path: str) -> None:
        try:
            entries = self.list(path)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return
            raise
        for entry in entries:
            child = f"{path.rstrip('/')}/{entry['name']}"
            if entry["type"] == "directory":
                self.delete_tree(child)
            else:
                with self.request("DELETE", "files", child):
                    pass
        with self.request("DELETE", "directories", path):
            pass


def normalize_mount(value: str) -> Path:
    if os.name == "nt" and len(value) == 2 and value[1] == ":":
        value += "\\"
    mount = Path(value).expanduser()
    if not mount.is_absolute():
        mount = Path.cwd() / mount
    return mount


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as source:
        while chunk := source.read(CHUNK_SIZE):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def deterministic_bytes(size: int, seed: int) -> bytes:
    generator = random.Random(seed)
    return generator.randbytes(size)


def write_pattern_file(path: Path, size: int, seed: int) -> tuple[str, int]:
    digest = hashlib.sha256()
    generator = random.Random(seed)
    remaining = size
    with path.open("wb") as output:
        while remaining:
            chunk = generator.randbytes(min(CHUNK_SIZE, remaining))
            output.write(chunk)
            digest.update(chunk)
            remaining -= len(chunk)
        output.flush()
        os.fsync(output.fileno())
    return digest.hexdigest(), size


def wait_remote_hash(
    api: ApiClient,
    remote_path: str,
    expected_hash: str,
    expected_size: int,
    deadline: float,
) -> None:
    last_problem = "not checked"
    while time.monotonic() < deadline:
        try:
            metadata = api.metadata(remote_path)
            if metadata.get("size") != expected_size:
                last_problem = f"size {metadata.get('size')} != {expected_size}"
            else:
                actual_hash, actual_size = api.hash_file(remote_path)
                if actual_size == expected_size and actual_hash == expected_hash:
                    return
                last_problem = (
                    f"hash/size {actual_hash}/{actual_size} != "
                    f"{expected_hash}/{expected_size}"
                )
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            last_problem = "HTTP 404"
        except (TimeoutError, urllib.error.URLError) as error:
            last_problem = str(error)
        time.sleep(0.1)
    raise TestFailure(f"remote verification timed out for {remote_path}: {last_problem}")


def verify_many_remote(
    api: ApiClient,
    expected: Iterable[tuple[str, str, int]],
    timeout: float,
    workers: int,
) -> None:
    deadline = time.monotonic() + timeout
    items = list(expected)
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [
            executor.submit(wait_remote_hash, api, path, digest, size, deadline)
            for path, digest, size in items
        ]
        for future in concurrent.futures.as_completed(futures):
            future.result()


def assert_mounted_hash(path: Path, expected_hash: str, expected_size: int) -> None:
    actual_hash, actual_size = sha256_file(path)
    if (actual_hash, actual_size) != (expected_hash, expected_size):
        raise TestFailure(
            f"mounted verification failed for {path}: "
            f"{actual_hash}/{actual_size} != {expected_hash}/{expected_size}"
        )


def section(name: str):
    class Timer:
        def __enter__(self):
            print(f"\n[{name}]", flush=True)
            self.started = time.monotonic()
            return self

        def __exit__(self, exc_type, exc, traceback):
            elapsed = time.monotonic() - self.started
            status = "PASS" if exc is None else "FAIL"
            print(f"[{status}] {name}: {elapsed:.3f}s", flush=True)
            return False

    return Timer()


def test_empty_and_boundary_files(
    mount_root: Path, remote_root: str, api: ApiClient, timeout: float, workers: int
) -> None:
    expected: list[tuple[str, str, int]] = []
    sizes = [0, 1, 4095, 4096, 4097, 65535, 65536, 65537, 1024 * 1024 + 123]
    directory = mount_root / "boundaries"
    directory.mkdir()
    for index, size in enumerate(sizes):
        data = deterministic_bytes(size, 10_000 + index)
        filename = f"size-{size}.bin"
        (directory / filename).write_bytes(data)
        expected.append(
            (f"{remote_root}/boundaries/{filename}", sha256_bytes(data), len(data))
        )
    verify_many_remote(api, expected, timeout, workers)
    for remote_path, digest, size in expected:
        assert_mounted_hash(mount_root / Path(remote_path).relative_to(remote_root), digest, size)


def test_tiny_files(
    mount_root: Path,
    remote_root: str,
    api: ApiClient,
    count: int,
    timeout: float,
    workers: int,
) -> None:
    directory = mount_root / "tiny"
    directory.mkdir()
    expected: list[tuple[str, str, int]] = []
    started = time.monotonic()
    for index in range(count):
        data = f"tiny payload {index}\n".encode()
        filename = f"file-{index:04d}.txt"
        (directory / filename).write_bytes(data)
        expected.append((f"{remote_root}/tiny/{filename}", sha256_bytes(data), len(data)))
    local_elapsed = time.monotonic() - started
    print(
        f"local completion: {local_elapsed:.3f}s "
        f"({count / max(local_elapsed, 0.000001):.2f} files/s)",
        flush=True,
    )
    verify_many_remote(api, expected, timeout, workers)
    durable_elapsed = time.monotonic() - started
    print(
        f"server durability: {durable_elapsed:.3f}s "
        f"({count / max(durable_elapsed, 0.000001):.2f} files/s)",
        flush=True,
    )


def test_special_names(mount_root: Path, remote_root: str, api: ApiClient, timeout: float) -> None:
    directory = mount_root / "names"
    directory.mkdir()
    names = ["caffè 東京 # %.txt", "leading space.txt", "emoji-🧪.bin"]
    for index, name in enumerate(names):
        data = deterministic_bytes(1024 + index, 20_000 + index)
        path = directory / name
        path.write_bytes(data)
        wait_remote_hash(
            api,
            f"{remote_root}/names/{name}",
            sha256_bytes(data),
            len(data),
            time.monotonic() + timeout,
        )


def test_overwrite_append_truncate(
    mount_root: Path, remote_root: str, api: ApiClient, timeout: float
) -> None:
    path = mount_root / "mutations.bin"
    initial = deterministic_bytes(2 * 1024 * 1024, 30_001)
    path.write_bytes(initial)
    wait_remote_hash(
        api,
        f"{remote_root}/mutations.bin",
        sha256_bytes(initial),
        len(initial),
        time.monotonic() + timeout,
    )

    shorter = deterministic_bytes(7777, 30_002)
    path.write_bytes(shorter)
    wait_remote_hash(
        api,
        f"{remote_root}/mutations.bin",
        sha256_bytes(shorter),
        len(shorter),
        time.monotonic() + timeout,
    )

    suffix = b"append-check" * 257
    with path.open("ab") as output:
        output.write(suffix)
        output.flush()
        os.fsync(output.fileno())
    combined = shorter + suffix
    wait_remote_hash(
        api,
        f"{remote_root}/mutations.bin",
        sha256_bytes(combined),
        len(combined),
        time.monotonic() + timeout,
    )

    with path.open("r+b") as output:
        output.truncate(len(combined) + 8192)
        output.flush()
        os.fsync(output.fileno())
    grown = combined + bytes(8192)
    wait_remote_hash(
        api,
        f"{remote_root}/mutations.bin",
        sha256_bytes(grown),
        len(grown),
        time.monotonic() + timeout,
    )


def test_random_access(mount_root: Path, remote_root: str, api: ApiClient, timeout: float) -> None:
    path = mount_root / "random-access.bin"
    expected = bytearray(deterministic_bytes(2 * 1024 * 1024, 40_001))
    path.write_bytes(expected)
    wait_remote_hash(
        api,
        f"{remote_root}/random-access.bin",
        sha256_bytes(expected),
        len(expected),
        time.monotonic() + timeout,
    )

    generator = random.Random(40_002)
    with path.open("r+b", buffering=0) as output:
        for _ in range(64):
            offset = generator.randrange(0, len(expected) - 4096)
            data = generator.randbytes(4096)
            output.seek(offset)
            output.write(data)
            expected[offset : offset + len(data)] = data
        os.fsync(output.fileno())
    digest = sha256_bytes(expected)
    assert_mounted_hash(path, digest, len(expected))
    wait_remote_hash(
        api,
        f"{remote_root}/random-access.bin",
        digest,
        len(expected),
        time.monotonic() + timeout,
    )


def test_rename_and_delete(mount_root: Path, remote_root: str, api: ApiClient, timeout: float) -> None:
    directory = mount_root / "rename"
    directory.mkdir()
    source = directory / "source.txt"
    destination = directory / "destination.txt"
    replacement = directory / "replacement.txt"
    source_data = b"source-data"
    destination_data = b"destination-data"
    source.write_bytes(source_data)
    source.replace(destination)
    wait_remote_hash(
        api,
        f"{remote_root}/rename/destination.txt",
        sha256_bytes(source_data),
        len(source_data),
        time.monotonic() + timeout,
    )

    replacement.write_bytes(destination_data)
    wait_remote_hash(
        api,
        f"{remote_root}/rename/replacement.txt",
        sha256_bytes(destination_data),
        len(destination_data),
        time.monotonic() + timeout,
    )
    replacement.replace(destination)
    wait_remote_hash(
        api,
        f"{remote_root}/rename/destination.txt",
        sha256_bytes(destination_data),
        len(destination_data),
        time.monotonic() + timeout,
    )

    nonempty = mount_root / "nonempty"
    nonempty.mkdir()
    (nonempty / "child.txt").write_bytes(b"child")
    try:
        nonempty.rmdir()
    except OSError:
        pass
    else:
        raise TestFailure("removing a non-empty directory unexpectedly succeeded")


def test_concurrent_files(
    mount_root: Path,
    remote_root: str,
    api: ApiClient,
    count: int,
    workers: int,
    timeout: float,
) -> None:
    directory = mount_root / "concurrent"
    directory.mkdir()

    def create(index: int) -> tuple[str, str, int]:
        data = deterministic_bytes(256 * 1024 + index, 50_000 + index)
        filename = f"worker-{index:03d}.bin"
        (directory / filename).write_bytes(data)
        return f"{remote_root}/concurrent/{filename}", sha256_bytes(data), len(data)

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        expected = list(executor.map(create, range(count)))
    verify_many_remote(api, expected, timeout, workers)


def test_large_file(
    mount_root: Path, remote_root: str, api: ApiClient, size_mib: int, timeout: float
) -> None:
    if size_mib <= 0:
        print("skipped (use --large-mib to enable)")
        return
    path = mount_root / "large.bin"
    expected_hash, expected_size = write_pattern_file(
        path, size_mib * 1024 * 1024, 60_001
    )
    wait_remote_hash(
        api,
        f"{remote_root}/large.bin",
        expected_hash,
        expected_size,
        time.monotonic() + timeout,
    )
    assert_mounted_hash(path, expected_hash, expected_size)


def test_sparse_file(
    mount_root: Path, remote_root: str, api: ApiClient, size_mib: int, timeout: float
) -> None:
    if size_mib <= 0:
        print("skipped (use --sparse-mib to enable)")
        return
    path = mount_root / "sparse.bin"
    size = size_mib * 1024 * 1024
    with path.open("wb") as output:
        output.seek(size - 1)
        output.write(b"Z")
        output.flush()
        os.fsync(output.fileno())
    expected = hashlib.sha256()
    zero_chunk = bytes(CHUNK_SIZE)
    remaining = size - 1
    while remaining:
        chunk = zero_chunk[: min(len(zero_chunk), remaining)]
        expected.update(chunk)
        remaining -= len(chunk)
    expected.update(b"Z")
    wait_remote_hash(
        api,
        f"{remote_root}/sparse.bin",
        expected.hexdigest(),
        size,
        time.monotonic() + timeout,
    )


def run_suite(args: argparse.Namespace) -> int:
    mount = normalize_mount(args.mount)
    if not mount.is_dir():
        raise TestFailure(f"mount does not exist or is not a directory: {mount}")
    api = ApiClient(args.server_url, args.token, args.request_timeout)
    api.list("")
    remote_root = f"e2e-{socket.gethostname()}-{uuid.uuid4().hex}"
    mount_root = mount / remote_root
    mount_root.mkdir()
    failed = False
    started = time.monotonic()
    try:
        with section("empty files and boundary sizes"):
            test_empty_and_boundary_files(
                mount_root, remote_root, api, args.durability_timeout, args.workers
            )
        with section(f"{args.tiny_count} tiny files"):
            test_tiny_files(
                mount_root,
                remote_root,
                api,
                args.tiny_count,
                args.durability_timeout,
                args.workers,
            )
        with section("Unicode and reserved URL characters"):
            test_special_names(mount_root, remote_root, api, args.durability_timeout)
        with section("overwrite, truncate, growth, and append"):
            test_overwrite_append_truncate(
                mount_root, remote_root, api, args.durability_timeout
            )
        with section("random-access writes"):
            test_random_access(mount_root, remote_root, api, args.durability_timeout)
        with section("rename, replacement, and delete validation"):
            test_rename_and_delete(mount_root, remote_root, api, args.durability_timeout)
        with section(f"{args.concurrent_count} concurrent files"):
            test_concurrent_files(
                mount_root,
                remote_root,
                api,
                args.concurrent_count,
                args.workers,
                args.durability_timeout,
            )
        with section(f"large file ({args.large_mib} MiB)"):
            test_large_file(
                mount_root,
                remote_root,
                api,
                args.large_mib,
                max(args.durability_timeout, args.large_mib * 4.0),
            )
        with section(f"sparse file ({args.sparse_mib} MiB logical)"):
            test_sparse_file(
                mount_root,
                remote_root,
                api,
                args.sparse_mib,
                max(args.durability_timeout, args.sparse_mib * 4.0),
            )
    except Exception:
        failed = True
        raise
    finally:
        if args.keep or (failed and not args.cleanup_on_failure):
            print(f"kept remote test tree: /{remote_root}")
        else:
            try:
                api.delete_tree(remote_root)
            except Exception as error:
                print(f"WARNING: cleanup failed for /{remote_root}: {error}", file=sys.stderr)
    print(f"\nE2E STRESS PASS in {time.monotonic() - started:.3f}s")
    return 0


def prepare_recovery(args: argparse.Namespace) -> int:
    mount = normalize_mount(args.mount)
    api = ApiClient(args.server_url, args.token, args.request_timeout)
    remote_root = f"recovery-{socket.gethostname()}-{uuid.uuid4().hex}"
    mount_root = mount / remote_root
    mount_root.mkdir()
    expected = []
    for index in range(args.recovery_count):
        size = 0 if index == 0 else 4096 + index
        data = deterministic_bytes(size, 70_000 + index)
        filename = f"pending-{index:03d}.bin"
        (mount_root / filename).write_bytes(data)
        expected.append(
            {"path": f"{remote_root}/{filename}", "sha256": sha256_bytes(data), "size": len(data)}
        )
    if args.recovery_large_mib > 0:
        filename = "pending-large.bin"
        digest, size = write_pattern_file(
            mount_root / filename,
            args.recovery_large_mib * 1024 * 1024,
            80_000,
        )
        expected.append(
            {"path": f"{remote_root}/{filename}", "sha256": digest, "size": size}
        )
    state = {
        "server_url": args.server_url,
        "mount": str(mount),
        "remote_root": remote_root,
        "files": expected,
        "created_by": f"{platform.system()} {platform.release()}",
    }
    Path(args.state_file).write_text(json.dumps(state, indent=2), encoding="utf-8")
    time.sleep(1.0)
    unexpectedly_uploaded = []
    for item in expected:
        try:
            api.metadata(item["path"])
            unexpectedly_uploaded.append(item["path"])
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
    if unexpectedly_uploaded:
        raise TestFailure(
            "recovery preparation requires the client to be mounted with "
            "REMOTE_FS_UPLOAD_PAUSED=1; some files reached the server: "
            + ", ".join(unexpectedly_uploaded[:3])
        )
    print(f"Prepared {len(expected)} durable pending files.")
    print(f"State file: {Path(args.state_file).resolve()}")
    print("Now force-kill the client, restart it without REMOTE_FS_UPLOAD_PAUSED, then run verify-recovery.")
    return 0


def verify_recovery(args: argparse.Namespace) -> int:
    state = json.loads(Path(args.state_file).read_text(encoding="utf-8"))
    mount = normalize_mount(args.mount or state["mount"])
    api = ApiClient(args.server_url or state["server_url"], args.token, args.request_timeout)
    expected = [(item["path"], item["sha256"], item["size"]) for item in state["files"]]
    verify_many_remote(api, expected, args.durability_timeout, args.workers)
    for remote_path, digest, size in expected:
        relative = Path(remote_path).relative_to(state["remote_root"])
        assert_mounted_hash(mount / state["remote_root"] / relative, digest, size)
    print(f"RECOVERY PASS: all {len(expected)} files survived forced client termination")
    if not args.keep:
        api.delete_tree(state["remote_root"])
        Path(args.state_file).unlink(missing_ok=True)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    def common(subparser: argparse.ArgumentParser, mount_required: bool = True) -> None:
        subparser.add_argument("--mount", required=mount_required, help="Mounted drive/directory, e.g. R: or /Volumes/remote")
        subparser.add_argument("--server-url", required=mount_required, help="Server base URL")
        subparser.add_argument("--token", default=os.environ.get("REMOTE_FS_TOKEN"))
        subparser.add_argument("--request-timeout", type=float, default=300.0)
        subparser.add_argument("--durability-timeout", type=float, default=300.0)
        subparser.add_argument("--workers", type=int, default=8)
        subparser.add_argument("--keep", action="store_true", help="Keep the isolated server test tree")

    run = subparsers.add_parser("run", help="Run the normal cross-platform stress suite")
    common(run)
    run.add_argument("--tiny-count", type=int, default=200)
    run.add_argument("--concurrent-count", type=int, default=64)
    run.add_argument("--large-mib", type=int, default=256)
    run.add_argument("--sparse-mib", type=int, default=0)
    run.add_argument(
        "--cleanup-on-failure",
        action="store_true",
        help="Delete confirmed server artifacts after failure (kept by default for diagnosis)",
    )
    run.set_defaults(handler=run_suite)

    prepare = subparsers.add_parser("prepare-recovery", help="Create paused durable journal entries before a forced kill")
    common(prepare)
    prepare.add_argument("--state-file", default="remote-fs-recovery-state.json")
    prepare.add_argument("--recovery-count", type=int, default=32)
    prepare.add_argument(
        "--recovery-large-mib",
        type=int,
        default=16,
        help="Size of an additional multi-chunk pending file (0 disables it)",
    )
    prepare.set_defaults(handler=prepare_recovery)

    verify = subparsers.add_parser("verify-recovery", help="Verify replay after restarting the client")
    common(verify, mount_required=False)
    verify.add_argument("--state-file", default="remote-fs-recovery-state.json")
    verify.set_defaults(handler=verify_recovery)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        return args.handler(args)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    except Exception as error:
        print(f"E2E STRESS FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
