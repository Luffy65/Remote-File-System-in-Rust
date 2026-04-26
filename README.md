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
- [ ] Read files from the remote server
- [ ] Write modified files back to the remote server
- [ ] Support creation, deletion, and renaming of files and directories
- [ ] Maintain file attributes such as size, timestamps, and permissions (as feasible)
- [ ] Run as a background daemon process that handles filesystem operations continuously

### Server Interface and implementation

The server should offer a set RESTful API for file operations:

- GET /list/`path` – List directory contents
- GET /files/`path` – Read file contents
- PUT /files/`path` – Write file contents
- POST /mkdir/`path` – Create directory
- DELETE /files/`path` – Delete file or directory

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

## Notes

Logging (log::info!, log::debug!) is your friend during the development of a FUSE filesystem.

## TODO

### Server Endpoints

- [ ] `PUT /files/*path` – Write file contents (replace/create)
- [ ] `DELETE /files/*path` – Delete file or directory

### Client API Functions

- [ ] `write_file()` – Send file write requests
- [ ] `delete_file()` – Send file delete requests
- [ ] `rename_file()` – Send file rename requests

### FUSE Operations

- [ ] `write()` – Write file data
- [ ] `unlink()` – Delete files
- [ ] `rmdir()` – Delete directories
- [ ] `rename()` – Rename/move files

### Improvements

- [ ] Better file permission handling (currently hardcoded)
- [ ] Proper file modification timestamps (currently `SystemTime::now()`)
- [ ] Graceful shutdown with signal handling
- [ ] File handle tracking for proper resource management
- [ ] Better error handling and logging
