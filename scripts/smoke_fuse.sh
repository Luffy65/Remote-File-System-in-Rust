#!/usr/bin/env bash
set -euo pipefail # If any checked command fails, it stops immediately and prints an error or a specific diagnostic.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_TMP="${SMOKE_TMPDIR:-/tmp}"
if [ -d /private/tmp ]; then
    BASE_TMP="${SMOKE_TMPDIR:-/private/tmp}"
fi
TMP_DIR="$(mktemp -d "$BASE_TMP/remote-fs-smoke.XXXXXX")"
STORAGE_DIR="$TMP_DIR/storage"
MOUNT_DIR="$TMP_DIR/mnt"
SERVER_LOG="$TMP_DIR/server.log"
CLIENT_LOG="$TMP_DIR/client.log"
SERVER_PID=""
CLIENT_PID=""

cleanup() {
    if command -v fusermount >/dev/null 2>&1; then
        fusermount -u "$MOUNT_DIR" >/dev/null 2>&1 || true
    else
        umount "$MOUNT_DIR" >/dev/null 2>&1 || true
    fi

    if [ -n "$CLIENT_PID" ]; then
        kill "$CLIENT_PID" >/dev/null 2>&1 || true
    fi
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" >/dev/null 2>&1 || true
    fi

    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

wait_for_server() {
    for _ in $(seq 1 40); do
        if ! kill -0 "$SERVER_PID" >/dev/null 2>&1; then
            echo "server exited early; log follows:" >&2
            cat "$SERVER_LOG" >&2 || true
            return 1
        fi
        if curl -fsS "http://127.0.0.1:3000/list/" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.25
    done

    echo "server did not become ready; log follows:" >&2
    cat "$SERVER_LOG" >&2 || true
    return 1
}

wait_for_mount() {
    local probe="mount_probe"

    for _ in $(seq 1 40); do
        if mkdir "$MOUNT_DIR/$probe" >/dev/null 2>&1; then
            if [ -d "$STORAGE_DIR/$probe" ]; then
                rmdir "$MOUNT_DIR/$probe" >/dev/null 2>&1 || true
                return 0
            fi
            rmdir "$MOUNT_DIR/$probe" >/dev/null 2>&1 || true
        fi
        if [ -d "$STORAGE_DIR/$probe" ]; then
            rmdir "$MOUNT_DIR/$probe" >/dev/null 2>&1 || true
            return 0
        fi
        sleep 0.25
    done

    echo "client did not mount; log follows:" >&2
    cat "$CLIENT_LOG" >&2 || true
    return 1
}

cd "$ROOT_DIR"
mkdir -p "$STORAGE_DIR" "$MOUNT_DIR"
cargo build -p server -p client

RUST_LOG=info target/debug/server "$STORAGE_DIR" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_for_server

RUST_LOG=info target/debug/client --daemon "$MOUNT_DIR" http://127.0.0.1:3000 >"$CLIENT_LOG" 2>&1
wait_for_mount

ls "$MOUNT_DIR" >/dev/null
mkdir "$MOUNT_DIR/docs"
printf 'hello remote fs' >"$MOUNT_DIR/docs/hello.txt"
test "$(cat "$MOUNT_DIR/docs/hello.txt")" = "hello remote fs"
test "$(cat "$STORAGE_DIR/docs/hello.txt")" = "hello remote fs"
mv "$MOUNT_DIR/docs/hello.txt" "$MOUNT_DIR/docs/renamed.txt"
test "$(cat "$MOUNT_DIR/docs/renamed.txt")" = "hello remote fs"
rm "$MOUNT_DIR/docs/renamed.txt"
rmdir "$MOUNT_DIR/docs"
test ! -e "$STORAGE_DIR/docs"

echo "FUSE smoke test passed"
