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

✅ Linux – Full support using FUSE (libfuse, fuser, or async-fuse)\
⚪ macOS – Optional support using macFUSE (best effort, no guarantee of full stability)\
⚪ Windows – Optional support using WinFSP or Dokany with C bindings (lower priority)

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

### Client

In another terminal, create a mount point and start the client:

```sh
mkdir test_folder
cargo run -p client -- --daemon test_folder http://127.0.0.1:3000
```

If the client runs on another PC or VM, replace `127.0.0.1` with the server machine address.

The mounted directory can then be used with normal file commands such as `ls`, `cat`, `mkdir`, `mv`, and `rm`.\
When finished, unmount it with `fusermount -u test_folder` on Linux or `umount test_folder` on macOS.\
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
