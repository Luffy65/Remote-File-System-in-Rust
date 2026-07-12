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

### Server

First, start the server. The current Rust server stores files on local disk under a configured storage root. Pass it as the first server argument, set `REMOTE_FS_ROOT`, or let it default to `./remote-storage`:

```sh
cargo run -p server -- ./remote-storage
```

For safety, the server listens only on `127.0.0.1:3000` by default. Local clients do not require authentication.

### Client

In another terminal, create a mount point and start the client. If you are on Unix:

```sh
mkdir test_folder
cargo run -p client -- --daemon test_folder http://127.0.0.1:3000
```

On Windows, install [WinFSP](https://winfsp.dev/rel/) before building the client. In the WinFSP installer, select the **Developer** component so that the required headers and libraries are installed. Building the Rust bindings also requires [LLVM/libclang](https://rust-lang.github.io/rust-bindgen/requirements.html); if it is not discovered automatically, set `LIBCLANG_PATH` to the LLVM `bin` directory containing `libclang.dll`. Then open a new terminal and mount the remote filesystem to a drive letter:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin" # Only needed when libclang is not auto-detected.
cargo build -p client
cargo run -p client -- R: http://127.0.0.1:3000
```

If the client runs on another PC or VM, replace `127.0.0.1` with the server machine address.

### Authenticated remote access

To accept remote clients, set a non-loopback listen address and a strong shared bearer token. The server refuses to start on a non-loopback address without a token:

```powershell
$env:REMOTE_FS_ADDR = "0.0.0.0:3000"
$env:REMOTE_FS_TOKEN = "replace-with-a-long-random-token"
cargo run -p server -- ./remote-storage
```

Set the same token in the client terminal:

```powershell
$env:REMOTE_FS_TOKEN = "replace-with-a-long-random-token"
cargo run -p client -- R: http://SERVER_ADDRESS:3000
```

The built-in server uses plain HTTP, so a bearer token protects access but not network confidentiality. Across an untrusted network, place the server behind TLS, an SSH tunnel, or a VPN. When the server runs on Windows, allow TCP port 3000 through Windows Firewall only for the networks and machines that need access.

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
