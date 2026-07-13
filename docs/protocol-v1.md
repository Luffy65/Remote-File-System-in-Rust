# Remote FS HTTP protocol v1

This document describes the current client/server contract. Shared Rust wire
types and header names live in the `remote-fs-protocol` workspace crate.

## Versioning and compatibility

Clients send `X-Remote-Fs-Protocol-Version: 1`; the server adds the same header
to every authenticated response. Version 1 servers still accept requests that
omit the header so older clients remain compatible. The header is currently
informational: the server does not yet reject another value. A future breaking
wire change must use a new major value and add explicit negotiation/rejection;
additive JSON fields should remain optional to preserve v1 compatibility.

## Transport, authentication, and paths

- The development server speaks HTTP. Use TLS or an encrypted tunnel/VPN across
  an untrusted network.
- When `REMOTE_FS_TOKEN` is configured, every request must contain
  `Authorization: Bearer TOKEN`; otherwise the response is `401`.
- Paths are storage-root-relative UTF-8 URL path components. Each component
  must be percent-encoded independently so `/` continues to represent
  hierarchy; a leading slash in a JSON path is ignored.
- `.`/`..`, the root as a mutation target, symbolic links, Windows reparse
  points, and `.remote-fs-transactions` are rejected.

## JSON types

`RemoteMetadata`:

```json
{"type":"file","size":12,"modified_at":"1710000000","mode":420,"uid":1000,"gid":1000}
```

`DirectoryEntry` adds `name` to the same fields. `type` is `file` or
`directory`; directory size is `0`; `modified_at` is Unix seconds encoded as a
decimal string. `mode`, `uid`, and `gid` are nullable for portability.

`POST /rename` accepts:

```json
{"from":"old/path","to":"new/path","replace_if_exists":true}
```

`replace_if_exists` defaults to `true` when omitted.

## Endpoints

| Request | Purpose and request data | Success |
| --- | --- | --- |
| `GET /list/` | List the storage root. | `200`, JSON array of `DirectoryEntry` |
| `GET /list/{path}` | List a directory. | `200`, JSON array of `DirectoryEntry` |
| `GET /files/{path}` | Stream a file, optionally using read-range headers. | `200`, raw bytes |
| `PUT /files/{path}` | Create/write/resize a file using headers below. Missing parent directories are created for normal writes. | `200`, `RemoteMetadata` |
| `PUT /files/{path}` + `If-None-Match: *` | Durable, atomic create-only upload; offsets/truncation are forbidden. | `201`, `RemoteMetadata` |
| `POST /mkdir/{path}` | Create exactly one directory and apply optional metadata headers. | `201`, `RemoteMetadata` |
| `GET /metadata/{path}` | Read metadata for a file or directory. | `200`, `RemoteMetadata` |
| `PATCH /metadata/{path}` | Apply any supplied metadata headers. | `200`, `RemoteMetadata` |
| `DELETE /files/{path}` | Delete a file. | `204`, empty body |
| `DELETE /directories/{path}` | Delete an empty directory. | `204`, empty body |
| `POST /rename` | Rename/move using the JSON body above. Parent directories are created. | `200`, empty body |

## Operation headers

| Header | Applies to | Meaning |
| --- | --- | --- |
| `X-File-Offset` | `GET/PUT /files` | Unsigned byte offset; default `0`. |
| `X-File-Size` | `GET /files` | Maximum number of bytes returned; omitted means to EOF. |
| `X-File-Truncate` | `PUT /files` | Resize to this unsigned length; the request body is ignored. |
| `X-File-Mode` | `PUT /files`, `POST /mkdir`, `PATCH /metadata` | Octal permission bits from `0000` through `7777`. Windows maps this to read-only/writable semantics. |
| `X-File-Uid` | Metadata-capable mutations | Unsigned owner ID; applied on Unix and accepted as a no-op elsewhere. |
| `X-File-Gid` | Metadata-capable mutations | Unsigned group ID; applied on Unix and accepted as a no-op elsewhere. |
| `X-File-Mtime` | Metadata-capable mutations | Unsigned Unix timestamp in seconds. |
| `If-None-Match: *` | `PUT /files` | Select atomic create-only behavior. |

An empty normal `PUT` at offset `0` creates or truncates the file. A non-empty
normal `PUT` overwrites bytes starting at the offset but does not truncate
trailing bytes. The server calls `sync_all` before acknowledging a write.

## Errors and failure modes

Errors use a UTF-8 plain-text body. The stable status categories are:

| Status | Meaning |
| --- | --- |
| `400 Bad Request` | Invalid path/header/body, wrong entry type, unsupported file type, or invalid rename. |
| `401 Unauthorized` | Missing or incorrect bearer token. |
| `403 Forbidden` | Permission failure, reserved internal path, symlink, or reparse point. |
| `404 Not Found` | Requested source/path/parent does not exist. |
| `409 Conflict` | Existing destination, non-empty directory, or another filesystem conflict. |
| `412 Precondition Failed` | Atomic create destination already exists. The destination is unchanged. |
| `500 Internal Server Error` | Unexpected storage failure; details remain in server logs. |

A disconnected normal offset write can have written a prefix before failure and
is not transactionally rolled back. The create-only upload is different: a
partial request remains in the private transaction area and is never published
at the destination. Clients may retry create-only uploads; they must compare an
existing destination before treating `412` as success.
