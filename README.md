# Remote File System in Rust

## Overview

Remote File System in Rust, project for the API Programming course at Politecnico di Torino, 2024/2025.

This project aims to implement a remote file system client in Rust that presents a local mount point, mirroring the structure of the file system hosted on a remote server. The file system should support transparent read and write access to remote files.

## Goals

- Provide a local file system interface that interacts with a remote storage backend.
- Enable standard file operations (read, write, create, delete, etc.) on remote files as if they were local.
- Ensure compatibility with Linux systems.
- Optionally support Windows and macOS with best-effort implementation.

## Functional Requirements

### Core Functionality

- [x] Mount a virtual file system to a local path (e.g., /mnt/remote-fs )
- [x] Display directories and files from a remote source
- [x] Read files from the remote server
- [x] Write modified files back to the remote server
- [x] Support creation, deletion, and renaming of files and directories
- [x] Maintain file attributes such as size, timestamps, and permissions (as feasible)
- [x] Run as a background daemon process that handles filesystem operations continuously

### Server Interface and implementation

The server should offer a set RESTful API for file operations:

- GET /list/`path` – List directory contents
- GET /files/`path` – Read file contents
- PUT /files/`path` – Write file contents
- POST /mkdir/`path` – Create directory
- GET /metadata/`path` – Read metadata for one file or directory
- PATCH /metadata/`path` – Update supported metadata
- DELETE /files/`path` – Delete file
- DELETE /directories/`path` – Delete empty directory
- POST /rename – Rename or move a file/directory

The server can be implemented using any language or framework, but should be RESTful and stateless.

### Caching

- Optional local caching layer for performance
- Configurable cache invalidation strategy (e.g., TTL or LRU)

### Windows durable write journal

New Windows files are written to a durable local journal before WinFSP reports a successful write. A bounded background uploader then sends the complete file through the normal `PUT /files/path` endpoint using standard `If-None-Match: *` create-only semantics. The server streams the upload into a transaction file, flushes it to durable storage, and atomically commits it without overwriting a conflicting remote path.

This is not a tiny-file-specific server API. It applies to newly created files of any size and has two separate timing boundaries:

- **Local completion:** the data has been flushed to the client journal and can be recovered after a client crash or restart.
- **Server durability:** the server has flushed and atomically committed the file, after which the journal entry is removed.

If an upload fails, the journal is retained and retried when the client restarts. If the remote path exists with different contents, the client retains the journal and logs a conflict instead of silently overwriting either version. `REMOTE_FS_UPLOAD_CONCURRENCY` controls the bounded uploader concurrency and defaults to `8`.

The default Windows journal location is `%LOCALAPPDATA%\remote-fs\journal\server-HASH`. Set `REMOTE_FS_JOURNAL_DIR` before mounting to choose a different durable base directory. Do not place it on a temporary/RAM disk. Overwriting or shrinking a pending file uses a copy-on-write generation and can temporarily require approximately twice that file's local disk space; sequential growth stays in place. The guarantee assumes at least one journal/server disk remains readable; no software can preserve data after simultaneous physical loss of every copy.

## Non-Functional Requirements

### Platform Support

✅ Linux – Full client/server support using FUSE\
✅ macOS – Client support using macFUSE, plus server support\
✅ Windows – Server support, plus experimental client support using WinFSP

### Performance

- Support for large files (100MB+) with streaming read/write
- Reasonable latency (<500ms for operations under normal network conditions)

### Startup and Shutdown

- Graceful startup and shutdown procedures

## Usage

### Choose the correct setup

| Setup | Server listen address | Authentication | Client URL |
| --- | --- | --- | --- |
| Client and server on the same machine | Default: `127.0.0.1:3000` | Not required | `http://127.0.0.1:3000` |
| Client and server on different machines | `REMOTE_FS_ADDR=0.0.0.0:3000` | `REMOTE_FS_TOKEN` is required on both | `http://SERVER_LAN_IP:3000` |

`127.0.0.1` always means "this machine." Do not use it in the client URL when the server runs on another computer. `0.0.0.0` is only a server bind address; never use it as the client destination.

The server stores files under the path passed as its first argument, under `REMOTE_FS_ROOT`, or under `./remote-storage` by default.

### A. Same-machine quick start

Start the server:

```sh
cargo run -p server -- ./remote-storage
```

Start a Linux/macOS client in another terminal:

```sh
mkdir test_folder
cargo run -p client -- test_folder http://127.0.0.1:3000
```

Start a Windows client in another PowerShell terminal. Windows mounts must use a free drive letter such as `R:`, not a folder such as `test_folder`:

```powershell
cargo run -p client -- R: http://127.0.0.1:3000
```

### B. LAN quick start: server on one machine, client on another

On a macOS/Linux server, bind to the LAN and choose a strong shared token:

```sh
export REMOTE_FS_ADDR="0.0.0.0:3000"
export REMOTE_FS_TOKEN="replace-with-a-long-random-token"
cargo run -p server -- ./remote-storage
```

On a Windows server, use the PowerShell equivalent:

```powershell
$env:REMOTE_FS_ADDR = "0.0.0.0:3000"
$env:REMOTE_FS_TOKEN = "replace-with-a-long-random-token"
cargo run -p server -- .\remote-storage
```

The server refuses to start on a non-loopback address without `REMOTE_FS_TOKEN`. Allow incoming TCP port 3000 if the operating-system firewall prompts you.

On the Windows client, first verify that the server machine and port are reachable. Replace `192.168.1.19` with the server's actual LAN address:

```powershell
ping.exe 192.168.1.19
Test-NetConnection 192.168.1.19 -Port 3000
```

`TcpTestSucceeded` must be `True`. If ping works but the TCP test fails, verify the server bind address and firewall. On macOS/Linux, confirm that the server is listening on all interfaces:

```sh
lsof -nP -iTCP:3000 -sTCP:LISTEN
```

The output should show `*:3000` or `0.0.0.0:3000`, not only `127.0.0.1:3000`.

Set the same token on the Windows client, verify the API, and mount a free drive letter:

```powershell
$env:REMOTE_FS_TOKEN = "replace-with-a-long-random-token"
Invoke-RestMethod -Headers @{ Authorization = "Bearer $env:REMOTE_FS_TOKEN" } http://192.168.1.19:3000/list/
cargo run -p client -- R: http://192.168.1.19:3000
```

For the first run, keep the client in the foreground so errors remain visible. After it works, add `--daemon` before the drive letter:

```powershell
cargo run -p client -- --daemon R: http://192.168.1.19:3000
```

### Windows client prerequisites

Install [WinFSP](https://winfsp.dev/rel/) and select the **Developer** component. Building the Rust bindings also requires [LLVM/libclang](https://rust-lang.github.io/rust-bindgen/requirements.html). If `libclang.dll` is not discovered automatically, set `LIBCLANG_PATH` to the LLVM `bin` directory:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build -p client
```

The built-in server uses plain HTTP, so a bearer token controls access but does not encrypt network traffic. Across an untrusted network, place the server behind TLS, an SSH tunnel, or a VPN.

The mounted directory can then be used with normal file commands such as `ls`, `cat`, `mkdir`, `mv`, and `rm`.\
When finished, unmount it with `fusermount -u test_folder` on Linux, `umount test_folder` on macOS, or Ctrl-C on Windows.\
For foreground debugging, omit `--daemon`.

### Logs

When launching the client, use `RUST_LOG=info` for normal runtime logs, or `RUST_LOG=debug` for detailed FUSE/API logs:

```sh
RUST_LOG=debug cargo run -p client -- test_folder http://127.0.0.1:3000
```

### Smoke Test

Run the local FUSE smoke test (it automatically starts client and server) with:

```sh
./scripts/smoke_fuse.sh
```

It will run some commands to test if they work. If some error pops up, it will be printed. Otherwise, we will just see "FUSE smoke test passed"

### Cross-platform E2E stress test

[`scripts/e2e_stress.py`](scripts/e2e_stress.py) requires Python 3 but uses only its standard library. It runs the same mounted-filesystem test suite regardless of which operating system is the client or server. It verifies both mounted reads and server-side durability through the HTTP API, using SHA-256 for every payload. The suite covers empty and boundary-sized files, 200 tiny files, Unicode and URL-reserved names, overwrite/append/truncate, randomized writes, rename replacement, invalid non-empty directory deletion, concurrent files, a configurable large file, and an optional sparse file.

Start the server and mount the client before running it. Windows client with macOS server:

```powershell
$env:REMOTE_FS_TOKEN = "replace-with-your-token"
python .\scripts\e2e_stress.py run `
  --mount R: `
  --server-url http://192.168.1.19:3000 `
  --large-mib 256
```

macOS/Linux client with a Windows server uses the same script and arguments, only with a Unix mount path and the Windows server IP:

```sh
export REMOTE_FS_TOKEN="replace-with-your-token"
python3 ./scripts/e2e_stress.py run \
  --mount ./test_folder \
  --server-url http://WINDOWS_SERVER_IP:3000 \
  --large-mib 256
```

Use `--sparse-mib 1024` for a full 1 GiB sparse-file transfer test. Every run creates a unique isolated server directory and removes it after success. Add `--keep` to retain a successful run. Failed runs are retained for diagnosis unless `--cleanup-on-failure` is explicitly supplied.

#### Forced client-crash recovery test (Windows journal)

This deliberately leaves journal entries pending, kills the client, and verifies that the next client process replays every byte. First mount the Windows client with uploads paused:

```powershell
$env:REMOTE_FS_TOKEN = "replace-with-your-token"
$env:REMOTE_FS_UPLOAD_PAUSED = "1"
cargo run -p client -- R: http://192.168.1.19:3000
```

In another PowerShell terminal, create and hash the recovery payloads:

```powershell
$env:REMOTE_FS_TOKEN = "replace-with-your-token"
python .\scripts\e2e_stress.py prepare-recovery `
  --mount R: `
  --server-url http://192.168.1.19:3000 `
  --state-file .\remote-fs-recovery-state.json
```

By default this prepares 32 small files plus one 16 MiB multi-chunk file. Use
`--recovery-count` and `--recovery-large-mib` to make the crash workload larger,
or `--recovery-large-mib 0` to omit the large file.

Force-kill that client process, remove `REMOTE_FS_UPLOAD_PAUSED`, and remount normally:

```powershell
Stop-Process -Name client -Force
Remove-Item Env:REMOTE_FS_UPLOAD_PAUSED
cargo run -p client -- R: http://192.168.1.19:3000
```

Finally, verify that the restarted client replayed every journal entry and that the mounted and server hashes match:

```powershell
python .\scripts\e2e_stress.py verify-recovery `
  --mount R: `
  --server-url http://192.168.1.19:3000 `
  --state-file .\remote-fs-recovery-state.json
```

`REMOTE_FS_UPLOAD_PAUSED` exists only for this crash-recovery test; never enable it for normal use. The normal E2E suite is client/server platform-independent. The forced-crash phase currently targets the Windows write journal and should be extended to FUSE when the same write-behind mechanism is enabled there.
