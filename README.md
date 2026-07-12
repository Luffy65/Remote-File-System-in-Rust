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
