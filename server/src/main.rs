// This is a server for a remote file system.
// It provides a basic REST API for listing, reading, writing, creating,
// deleting, and renaming files/directories under a configured local root.
//
// To run the server, navigate to the project root and run:
// `cargo run -p server -- [STORAGE_ROOT]`
//
// The server listens on `127.0.0.1:3000` by default. Set `REMOTE_FS_ADDR`
// and `REMOTE_FS_TOKEN` explicitly to serve authenticated remote clients.

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header::AUTHORIZATION, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    env, io,
    io::SeekFrom,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};

const DEFAULT_STORAGE_ROOT: &str = "remote-storage";
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:3000";
const TRANSFER_BUFFER_SIZE: usize = 4 * 1024 * 1024;
#[cfg(not(unix))]
const DEFAULT_FILE_MODE: u32 = 0o644;
#[cfg(not(unix))]
const DEFAULT_DIR_MODE: u32 = 0o755;

struct AppState {
    root_dir: PathBuf,
    auth_token: Option<String>,
}

impl AppState {
    fn with_auth(root_dir: PathBuf, auth_token: Option<String>) -> Self {
        AppState {
            root_dir,
            auth_token,
        }
    }

    // Resolves an API path below the storage root and rejects traversal like `..`
    // as well as symlink/reparse-point components that could escape the root.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;
        self.reject_link_components(&relative_path)?;
        Ok(self.root_dir.join(relative_path))
    }

    // Same as `resolve_path`, but rejects the root path for mutating endpoints.
    fn resolve_non_root_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;

        if relative_path.as_os_str().is_empty() {
            return Err(StorageError::BadRequest("Path cannot be empty"));
        }

        self.reject_link_components(&relative_path)?;
        Ok(self.root_dir.join(relative_path))
    }

    fn reject_link_components(&self, relative_path: &Path) -> Result<(), StorageError> {
        let mut current = self.root_dir.clone();

        for component in relative_path.components() {
            let Component::Normal(part) = component else {
                return Err(StorageError::BadRequest("Invalid path"));
            };
            current.push(part);

            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if is_link_or_reparse_point(&metadata) => {
                    return Err(StorageError::Forbidden(
                        "Symbolic links and reparse points are not allowed",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(StorageError::from_io(error, "Could not inspect path"));
                }
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(not(unix), not(windows)))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[derive(Debug)]
enum StorageError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Forbidden(&'static str),
    Conflict(&'static str),
    RequestBody(&'static str),
    Io(io::Error),
}

impl StorageError {
    fn from_io(error: io::Error, not_found_message: &'static str) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => StorageError::NotFound(not_found_message),
            io::ErrorKind::AlreadyExists => StorageError::Conflict("Path already exists"),
            io::ErrorKind::PermissionDenied => StorageError::Forbidden("Permission denied"),
            io::ErrorKind::InvalidInput
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::IsADirectory => StorageError::BadRequest("Invalid path"),
            io::ErrorKind::DirectoryNotEmpty => StorageError::Conflict("Directory not empty"),
            _ => StorageError::Io(error),
        }
    }
}

impl IntoResponse for StorageError {
    fn into_response(self) -> Response {
        match self {
            StorageError::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            StorageError::NotFound(message) => (StatusCode::NOT_FOUND, message).into_response(),
            StorageError::Forbidden(message) => (StatusCode::FORBIDDEN, message).into_response(),
            StorageError::Conflict(message) => (StatusCode::CONFLICT, message).into_response(),
            StorageError::RequestBody(message) => {
                (StatusCode::BAD_REQUEST, message).into_response()
            }
            StorageError::Io(error) => {
                log::error!("Storage error: {error}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Storage error").into_response()
            }
        }
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    from: String,
    to: String,
    #[serde(default = "replace_if_exists_by_default")]
    replace_if_exists: bool,
}

fn replace_if_exists_by_default() -> bool {
    true
}

/// Represents a directory entry (file or directory).
#[derive(Serialize)]
struct DirectoryEntry {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    size: u64,
    modified_at: String,
    mode: u32,
    uid: u32,
    gid: u32,
}

/// Represents the metadata returned after create/write/setattr operations.
#[derive(Serialize)]
struct EntryMetadata {
    #[serde(rename = "type")]
    type_: String,
    size: u64,
    modified_at: String,
    mode: u32,
    uid: u32,
    gid: u32,
}

// Converts API paths to a safe relative filesystem path.
fn sanitize_api_path(path: &str) -> Result<PathBuf, StorageError> {
    let trimmed_path = path.trim_matches('/');
    let mut relative_path = PathBuf::new();

    if trimmed_path.is_empty() {
        return Ok(relative_path);
    }

    for component in Path::new(trimmed_path).components() {
        match component {
            Component::Normal(part) => relative_path.push(part),
            _ => return Err(StorageError::BadRequest("Invalid path")),
        }
    }

    Ok(relative_path)
}

fn bearer_token_matches(header_value: &str, expected_token: &str) -> bool {
    let Some(provided_token) = header_value.strip_prefix("Bearer ") else {
        return false;
    };

    let provided = provided_token.as_bytes();
    let expected = expected_token.as_bytes();
    let mut difference = provided.len() ^ expected.len();
    let comparison_len = provided.len().max(expected.len());

    for index in 0..comparison_len {
        difference |= usize::from(
            provided.get(index).copied().unwrap_or(0) ^ expected.get(index).copied().unwrap_or(0),
        );
    }

    difference == 0
}

async fn require_authentication(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(expected_token) = state.auth_token.as_deref() else {
        return next.run(request).await;
    };

    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| bearer_token_matches(value, expected_token));

    if authorized {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "Missing or invalid bearer token").into_response()
    }
}

// Formats modification time as seconds since the Unix epoch.
fn format_modified_at(modified_at: SystemTime) -> String {
    modified_at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.is_dir() {
        DEFAULT_DIR_MODE
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        DEFAULT_FILE_MODE
    }
}

#[cfg(unix)]
fn metadata_uid(metadata: &std::fs::Metadata) -> u32 {
    metadata.uid()
}

#[cfg(not(unix))]
fn metadata_uid(_metadata: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn metadata_gid(metadata: &std::fs::Metadata) -> u32 {
    metadata.gid()
}

#[cfg(not(unix))]
fn metadata_gid(_metadata: &std::fs::Metadata) -> u32 {
    0
}

fn entry_type(metadata: &std::fs::Metadata) -> Option<String> {
    if metadata.is_dir() {
        Some("directory".to_string())
    } else if metadata.is_file() {
        Some("file".to_string())
    } else {
        None
    }
}

fn entry_metadata_from_metadata(metadata: std::fs::Metadata) -> Option<EntryMetadata> {
    let type_ = entry_type(&metadata)?;

    Some(EntryMetadata {
        type_,
        size: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        modified_at: metadata
            .modified()
            .map(format_modified_at)
            .unwrap_or_else(|_| "0".to_string()),
        mode: metadata_mode(&metadata),
        uid: metadata_uid(&metadata),
        gid: metadata_gid(&metadata),
    })
}

async fn entry_metadata_for_path(path: &Path) -> Result<EntryMetadata, StorageError> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    entry_metadata_from_metadata(metadata).ok_or(StorageError::BadRequest("Unsupported file type"))
}

// Builds the JSON entry returned by the directory listing endpoint.
fn directory_entry_from_metadata(
    name: String,
    metadata: std::fs::Metadata,
) -> Option<DirectoryEntry> {
    let entry_metadata = entry_metadata_from_metadata(metadata)?;

    Some(DirectoryEntry {
        name,
        type_: entry_metadata.type_,
        size: entry_metadata.size,
        modified_at: entry_metadata.modified_at,
        mode: entry_metadata.mode,
        uid: entry_metadata.uid,
        gid: entry_metadata.gid,
    })
}

// Parses an optional integer request header used for ranged file access.
fn parse_optional_u64_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<u64>, StorageError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<u64>().ok())
                .ok_or(StorageError::BadRequest("Invalid numeric header"))
        })
        .transpose()
}

fn parse_optional_u32_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<u32>, StorageError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .ok_or(StorageError::BadRequest("Invalid numeric header"))
        })
        .transpose()
}

fn parse_optional_mode_header(headers: &HeaderMap) -> Result<Option<u32>, StorageError> {
    headers
        .get("X-File-Mode")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| u32::from_str_radix(text, 8).ok())
                .filter(|mode| *mode <= 0o7777)
                .ok_or(StorageError::BadRequest("Invalid file mode"))
        })
        .transpose()
}

#[cfg(unix)]
fn path_to_cstring(path: &Path) -> Result<CString, io::Error> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
}

async fn apply_mode(path: &Path, mode: u32) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)
            .await
            .map_err(|error| StorageError::from_io(error, "Path not found"))?
            .permissions();
        permissions.set_mode(mode & 0o7777);
        fs::set_permissions(path, permissions)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not update permissions"))?;
    }

    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .await
            .map_err(|error| StorageError::from_io(error, "Path not found"))?
            .permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not update permissions"))?;
    }

    Ok(())
}

fn apply_owner(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<(), StorageError> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }

    #[cfg(not(unix))]
    let _ = (path, uid, gid);

    #[cfg(unix)]
    {
        let c_path =
            path_to_cstring(path).map_err(|error| StorageError::from_io(error, "Invalid path"))?;
        let uid = uid
            .map(|value| value as libc::uid_t)
            .unwrap_or(!0 as libc::uid_t);
        let gid = gid
            .map(|value| value as libc::gid_t)
            .unwrap_or(!0 as libc::gid_t);

        let result = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
        if result != 0 {
            return Err(StorageError::from_io(
                io::Error::last_os_error(),
                "Could not update owner",
            ));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn timespec_from_system_time(time: SystemTime) -> libc::timespec {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));

    libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

fn apply_modified_time(path: &Path, modified_at: u64) -> Result<(), StorageError> {
    #[cfg(windows)]
    {
        use std::fs::{FileTimes, OpenOptions as StdOpenOptions};
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        let requested_time = UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(modified_at))
            .ok_or(StorageError::BadRequest(
                "Modification time is out of range",
            ))?;
        let file = StdOpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|error| StorageError::from_io(error, "Could not open path"))?;

        file.set_times(FileTimes::new().set_modified(requested_time))
            .map_err(|error| StorageError::from_io(error, "Could not update modification time"))?;
    }

    #[cfg(all(not(unix), not(windows)))]
    let _ = (path, modified_at);

    #[cfg(unix)]
    {
        let c_path =
            path_to_cstring(path).map_err(|error| StorageError::from_io(error, "Invalid path"))?;
        let metadata = std::fs::metadata(path)
            .map_err(|error| StorageError::from_io(error, "Path not found"))?;
        let accessed_at = metadata.accessed().unwrap_or(UNIX_EPOCH);
        let times = [
            timespec_from_system_time(accessed_at),
            libc::timespec {
                tv_sec: modified_at as libc::time_t,
                tv_nsec: 0,
            },
        ];

        let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        if result != 0 {
            return Err(StorageError::from_io(
                io::Error::last_os_error(),
                "Could not update modification time",
            ));
        }
    }

    Ok(())
}

async fn apply_metadata_headers(path: &Path, headers: &HeaderMap) -> Result<(), StorageError> {
    if let Some(mode) = parse_optional_mode_header(headers)? {
        apply_mode(path, mode).await?;
    }

    let uid = parse_optional_u32_header(headers, "X-File-Uid")?;
    let gid = parse_optional_u32_header(headers, "X-File-Gid")?;
    apply_owner(path, uid, gid)?;

    if let Some(modified_at) = parse_optional_u64_header(headers, "X-File-Mtime")? {
        apply_modified_time(path, modified_at)?;
    }

    Ok(())
}

// Lists the immediate children of one directory on disk.
async fn list_entries(state: &AppState, path: &str) -> Result<Vec<DirectoryEntry>, StorageError> {
    let directory_path = state.resolve_path(path)?;
    let metadata = fs::metadata(&directory_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Directory not found"))?;

    if !metadata.is_dir() {
        return Err(StorageError::BadRequest("Path is not a directory"));
    }

    let mut read_dir = fs::read_dir(&directory_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Directory not found"))?;
    let mut entries = Vec::new();

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|error| StorageError::from_io(error, "Could not read directory"))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry
            .metadata()
            .await
            .map_err(|error| StorageError::from_io(error, "Could not read entry"))?;

        if let Some(directory_entry) = directory_entry_from_metadata(name, metadata) {
            entries.push(directory_entry);
        }
    }

    entries.sort_by(|left, right| left.name.cmp(&right.name));
    log::info!(
        "Listed /{} ({} entries)",
        path.trim_matches('/'),
        entries.len()
    );
    Ok(entries)
}

// Builds the Axum router with all file-system endpoints wired to shared state.
fn build_app(shared_state: Arc<AppState>) -> Router {
    let authentication_state = shared_state.clone();

    Router::new()
        .route("/list/", get(list_root))
        .route("/list/*path", get(list_path))
        .route(
            "/files/*path",
            get(get_file).put(write_file).delete(delete_path),
        )
        .route("/directories/*path", delete(delete_directory))
        .route("/metadata/*path", get(get_metadata).patch(update_metadata))
        .route("/mkdir/*path", post(make_directory))
        .route("/rename", post(rename_entry))
        .with_state(shared_state)
        .layer(middleware::from_fn_with_state(
            authentication_state,
            require_authentication,
        ))
}

// Reads the storage root from the first CLI argument, then env, then a default.
fn configured_storage_root() -> PathBuf {
    env::args()
        .nth(1)
        .or_else(|| env::var("REMOTE_FS_ROOT").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_ROOT))
}

fn configured_listen_addr() -> Result<SocketAddr, String> {
    let value = env::var("REMOTE_FS_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string());
    value
        .parse::<SocketAddr>()
        .map_err(|error| format!("Invalid REMOTE_FS_ADDR '{value}': {error}"))
}

fn configured_auth_token() -> Option<String> {
    env::var("REMOTE_FS_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

// Handler for `POST /mkdir/*path`: creates a directory on disk.
async fn make_directory(
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let directory_path = state.resolve_non_root_path(&path)?;

    fs::create_dir(&directory_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Parent directory not found"))?;

    apply_metadata_headers(&directory_path, &headers).await?;
    let metadata = entry_metadata_for_path(&directory_path).await?;
    log::info!("Created directory /{}", path.trim_matches('/'));

    Ok((StatusCode::CREATED, Json(metadata)))
}

// Handler for `GET /files/*path`: streams a whole file, or just one requested byte range.
async fn get_file(
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Response, StorageError> {
    let file_path = state.resolve_non_root_path(&path)?;
    let metadata = fs::metadata(&file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "File not found"))?;

    if !metadata.is_file() {
        return Err(StorageError::BadRequest("Path is not a file"));
    }

    let offset = parse_optional_u64_header(&headers, "X-File-Offset")?.unwrap_or(0);
    let requested_size = parse_optional_u64_header(&headers, "X-File-Size")?;

    if offset >= metadata.len() {
        log::info!(
            "Read /{} (offset: {}, bytes: 0)",
            path.trim_matches('/'),
            offset
        );
        return Ok((StatusCode::OK, Body::empty()).into_response());
    }

    let mut file = fs::File::open(&file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "File not found"))?;
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| StorageError::from_io(error, "Could not seek file"))?;

    let remaining_size = metadata.len() - offset;
    let response_size = requested_size
        .map(|size| size.min(remaining_size))
        .unwrap_or(remaining_size);
    log::info!(
        "Read /{} (offset: {}, bytes: {})",
        path.trim_matches('/'),
        offset,
        response_size
    );
    let stream = ReaderStream::with_capacity(file.take(response_size), TRANSFER_BUFFER_SIZE);

    Ok((StatusCode::OK, Body::from_stream(stream)).into_response())
}

// Handler for `PUT /files/*path`: streams the request body to disk at the requested offset.
async fn write_file(
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Body,
) -> Result<Response, StorageError> {
    let file_path = state.resolve_non_root_path(&path)?;
    let offset = parse_optional_u64_header(&headers, "X-File-Offset")?.unwrap_or(0);
    let truncate_size = parse_optional_u64_header(&headers, "X-File-Truncate")?;

    log::debug!(
        "Receiving streamed bytes for /{} (offset: {})",
        path.trim_matches('/'),
        offset
    );

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not create parent directory"))?;
    }

    let mut body_stream = body.into_data_stream();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not open file"))?;

    if let Some(size) = truncate_size {
        file.set_len(size)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not resize file"))?;
        file.flush()
            .await
            .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
        drop(file);
        apply_metadata_headers(&file_path, &headers).await?;
        let metadata = entry_metadata_for_path(&file_path).await?;
        log::info!("Resized /{} to {} bytes", path.trim_matches('/'), size);
        return Ok((StatusCode::OK, Json(metadata)).into_response());
    }

    let mut bytes_written = 0;
    let mut write_buffer = Vec::with_capacity(TRANSFER_BUFFER_SIZE);

    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| StorageError::from_io(error, "Could not seek file"))?;

    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk.map_err(|_| StorageError::RequestBody("Could not read request body"))?;
        bytes_written += chunk.len();
        if write_buffer.len() + chunk.len() > TRANSFER_BUFFER_SIZE && !write_buffer.is_empty() {
            file.write_all(&write_buffer)
                .await
                .map_err(|error| StorageError::from_io(error, "Could not write file"))?;
            write_buffer.clear();
        }

        if chunk.len() >= TRANSFER_BUFFER_SIZE {
            file.write_all(&chunk)
                .await
                .map_err(|error| StorageError::from_io(error, "Could not write file"))?;
        } else {
            write_buffer.extend_from_slice(&chunk);
        }
    }

    if !write_buffer.is_empty() {
        file.write_all(&write_buffer)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not write file"))?;
    }

    if offset == 0 && bytes_written == 0 {
        file.set_len(0)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not truncate file"))?;
        file.flush()
            .await
            .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
        drop(file);
        apply_metadata_headers(&file_path, &headers).await?;
        let metadata = entry_metadata_for_path(&file_path).await?;
        log::info!("Created or truncated /{}", path.trim_matches('/'));
        return Ok((StatusCode::OK, Json(metadata)).into_response());
    }

    file.flush()
        .await
        .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
    drop(file);

    apply_metadata_headers(&file_path, &headers).await?;
    let metadata = entry_metadata_for_path(&file_path).await?;
    log::info!(
        "Wrote /{} (offset: {}, bytes: {})",
        path.trim_matches('/'),
        offset,
        bytes_written
    );

    Ok((StatusCode::OK, Json(metadata)).into_response())
}

// Handler for `GET /metadata/*path`: returns metadata for exactly one path.
async fn get_metadata(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EntryMetadata>, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    let metadata = entry_metadata_for_path(&target_path).await?;
    log::debug!("Read metadata for /{}", path.trim_matches('/'));
    Ok(Json(metadata))
}

// Handler for `PATCH /metadata/*path`: updates mode, owner, or modification time.
async fn update_metadata(
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    fs::metadata(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    apply_metadata_headers(&target_path, &headers).await?;
    let metadata = entry_metadata_for_path(&target_path).await?;
    log::info!("Updated metadata for /{}", path.trim_matches('/'));

    Ok((StatusCode::OK, Json(metadata)))
}

// Handler for `DELETE /files/*path`: removes a file.
async fn delete_path(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    let metadata = fs::metadata(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    if !metadata.is_file() {
        return Err(StorageError::BadRequest("Path is not a file"));
    }

    fs::remove_file(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not remove file"))?;
    log::info!("Deleted file /{}", path.trim_matches('/'));

    Ok(StatusCode::NO_CONTENT)
}

// Handler for `DELETE /directories/*path`: removes an empty directory.
async fn delete_directory(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    let metadata = fs::metadata(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    if !metadata.is_dir() {
        return Err(StorageError::BadRequest("Path is not a directory"));
    }

    fs::remove_dir(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not remove directory"))?;
    log::info!("Deleted directory /{}", path.trim_matches('/'));

    Ok(StatusCode::NO_CONTENT)
}

// Handler for `POST /rename`: moves/renames files or directory trees.
async fn rename_entry(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RenameRequest>,
) -> Result<impl IntoResponse, StorageError> {
    let from_path = state.resolve_non_root_path(&payload.from)?;
    let to_path = state.resolve_non_root_path(&payload.to)?;

    if from_path == to_path {
        return Ok(StatusCode::OK);
    }

    let from_metadata = fs::metadata(&from_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Source path not found"))?;

    if from_metadata.is_dir() && to_path.starts_with(&from_path) {
        return Err(StorageError::BadRequest(
            "Cannot move a directory inside itself",
        ));
    }

    if !payload.replace_if_exists {
        match fs::symlink_metadata(&to_path).await {
            Ok(_) => return Err(StorageError::Conflict("Destination path already exists")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(StorageError::from_io(
                    error,
                    "Could not inspect destination path",
                ));
            }
        }
    }

    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not create parent directory"))?;
    }

    fs::rename(&from_path, &to_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not rename path"))?;
    log::info!(
        "Renamed /{} to /{}",
        payload.from.trim_matches('/'),
        payload.to.trim_matches('/')
    );

    Ok(StatusCode::OK)
}

// Handler for `GET /list/`: lists the root directory.
async fn list_root(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DirectoryEntry>>, StorageError> {
    Ok(Json(list_entries(&state, "").await?))
}

// Handler for `GET /list/*path`: lists a specific directory.
async fn list_path(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DirectoryEntry>>, StorageError> {
    Ok(Json(list_entries(&state, &path).await?))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => log::info!("Shutdown requested with Ctrl-C."),
        _ = terminate => log::info!("Shutdown requested with SIGTERM."),
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let storage_root = configured_storage_root();
    std::fs::create_dir_all(&storage_root).expect("Failed to create storage root");
    let storage_root =
        std::fs::canonicalize(&storage_root).expect("Failed to resolve storage root");
    let addr = configured_listen_addr().unwrap_or_else(|error| panic!("{error}"));
    let auth_token = configured_auth_token();

    if !addr.ip().is_loopback() && auth_token.is_none() {
        panic!("REMOTE_FS_TOKEN must be set when REMOTE_FS_ADDR listens on a non-loopback address");
    }

    let app = build_app(Arc::new(AppState::with_auth(
        storage_root.clone(),
        auth_token.clone(),
    )));
    log::info!("Server storage root: {}", storage_root.display());
    log::info!("Server listening on {}", addr);
    log::info!(
        "Bearer-token authentication: {}",
        if auth_token.is_some() {
            "enabled"
        } else {
            "disabled (loopback only)"
        }
    );

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

#[cfg(test)]
mod tests;
