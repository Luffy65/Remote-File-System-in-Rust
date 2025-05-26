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

Il logging (log::info!, log::debug!) è tuo amico durante lo sviluppo di un filesystem FUSE.

## TODO

### Phase 1: Core Setup & Basic Server Interaction

#### 🎯 Sub-Goal 4: Basic File Read (Read-Only)

* **Task:** Define the `GET /files/path` server endpoint for reading file content.
    * The mock server should be updated to serve predefined content for a specific file path (e.g., `GET /files/hello.txt` returns "Hello, World!").
* **Task:** In the Rust FUSE client, implement the `lookup` FUSE operation.
    * When a file is looked up, the client should (for now) just confirm its existence based on a simulated call or by checking if it was listed by `readdir`.
* **Task:** Implement the `getattr` FUSE operation.
    * This should return basic attributes (like file type, size, permissions) for files and directories. For now, the size can be hardcoded for the mock file or derived from the mock server's `/list` response.
* **Task:** Implement the `open` and `read` FUSE operations.
    * `open`: Can be a simple pass-through for now, ensuring the file type is regular.
    * `read`:
        1.  Receive a path and offset from the FUSE kernel.
        2.  Make an HTTP GET request to the mock server's `GET /files/path_from_fuse`.
        3.  Return the (mock) file content to the FUSE kernel via `reply.data()`.
* **Goal:** Be able to `cat /mnt/remote-fs/hello.txt` (or equivalent) and see the content served by your mock server.

---
