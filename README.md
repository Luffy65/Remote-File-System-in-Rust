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
- DELETE /files/`path` – Delete file or directory
- POST /rename – Rename or move a file/directory

The server can be implemented using any language or framework, but should be RESTful and stateless.

### Caching

- Optional local caching layer for performance, implemented in the client with a TTL/LRU directory cache
- Configurable cache invalidation strategy (TTL plus LRU eviction)

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

First, start the server. The current Rust server stores files on local disk under a configured storage root. Pass it as the
first server argument, set `REMOTE_FS_ROOT`, or let it default to `./remote-storage`:

```sh
cargo run -p server -- ./remote-storage
```

### Client

In another terminal, create a mount point and start the client:

```sh
mkdir test_folder
cargo run -p client -- test_folder http://127.0.0.1:3000
```

The mounted directory can then be used with normal file commands (from a third terminal) such as `ls`, `cat`, `mkdir`, `mv`, and `rm`.\
When finished, unmount it with `fusermount -u test_folder` on Linux or `umount test_folder` on macOS.

## Notes

Logging (log::info!, log::debug!) is your friend during the development of a FUSE filesystem.

## TODO

### Server Endpoints

- [x] `GET /list/*path` – List directory contents
- [x] `GET /files/*path` – Read file contents
- [x] `PUT /files/*path` – Write file contents (replace/create)
- [x] `POST /mkdir/*path` – Create directory
- [x] `DELETE /files/*path` – Delete file or directory
- [x] `POST /rename` – Rename or move file/directory

### Client API Functions

- [x] `list_directory()` – Send directory listing requests
- [x] `read_file()` – Send byte-range file read requests
- [x] `create_file()` – Send empty file creation requests
- [x] `create_directory()` – Send directory creation requests
- [x] `write_file()` – Send file write requests
- [x] `resize_file()` – Send file resize requests
- [x] `delete_file()` – Send file delete requests
- [x] `rename_file()` – Send file rename requests

### FUSE Operations

- [x] `lookup()` – Resolve paths to inodes
- [x] `getattr()` – Return file and directory attributes
- [x] `readdir()` – List directory entries
- [x] `read()` – Read file data
- [x] `create()` – Create files
- [x] `mkdir()` – Create directories
- [x] `write()` – Write file data
- [x] `unlink()` – Delete files
- [x] `rmdir()` – Delete directories
- [x] `rename()` – Rename/move files

### Improvements

- [x] Pass from a mock server to a real one: replace the in-memory HashMap/HashSet with a real backend (local disk storage under a configured root directory) (important)
- [x] Better file permission handling (currently hardcoded)
- [x] Proper file modification timestamps (modified_at) (currently we have `SystemTime::now()`)
- [x] Graceful shutdown with signal handling
- [x] File handle tracking for proper resource management
- [ ] Better error handling and logging
