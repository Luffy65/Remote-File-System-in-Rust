// This is a server for a remote file system.
// It provides a basic REST API for listing, reading, writing, creating,
// deleting, and renaming files/directories under a configured local root.
//
// To run the server, navigate to the project root and run:
// `cargo run -p server -- [STORAGE_ROOT]`
//
// The server will listen on `0.0.0.0:3000`.

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
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

const DEFAULT_STORAGE_ROOT: &str = "remote-storage";

struct AppState {
    root_dir: PathBuf,
}

impl AppState {
    fn new(root_dir: PathBuf) -> Self {
        AppState { root_dir }
    }

    // Resolves an API path below the storage root and rejects traversal like `..`.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;
        Ok(self.root_dir.join(relative_path))
    }

    // Same as `resolve_path`, but rejects the root path for mutating endpoints.
    fn resolve_non_root_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;

        if relative_path.as_os_str().is_empty() {
            return Err(StorageError::BadRequest("Path cannot be empty"));
        }

        Ok(self.root_dir.join(relative_path))
    }
}

#[derive(Debug)]
enum StorageError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Conflict(&'static str),
    RequestBody(&'static str),
    Io(io::Error),
}

impl StorageError {
    fn from_io(error: io::Error, not_found_message: &'static str) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => StorageError::NotFound(not_found_message),
            io::ErrorKind::AlreadyExists => StorageError::Conflict("Path already exists"),
            _ => StorageError::Io(error),
        }
    }
}

impl IntoResponse for StorageError {
    fn into_response(self) -> Response {
        match self {
            StorageError::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            StorageError::NotFound(message) => (StatusCode::NOT_FOUND, message).into_response(),
            StorageError::Conflict(message) => (StatusCode::CONFLICT, message).into_response(),
            StorageError::RequestBody(message) => {
                (StatusCode::BAD_REQUEST, message).into_response()
            }
            StorageError::Io(error) => {
                eprintln!("Storage error: {error}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Storage error").into_response()
            }
        }
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    from: String,
    to: String,
}

/// Represents a directory entry (file or directory).
#[derive(Serialize)]
struct DirectoryEntry {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    size: u64,
    modified_at: String,
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

// Formats modification time as seconds since the Unix epoch.
fn format_modified_at(modified_at: SystemTime) -> String {
    modified_at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

// Builds the JSON entry returned by the directory listing endpoint.
fn directory_entry_from_metadata(
    name: String,
    metadata: std::fs::Metadata,
) -> Option<DirectoryEntry> {
    let type_ = if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        return None;
    };

    Some(DirectoryEntry {
        name,
        type_: type_.to_string(),
        size: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        modified_at: metadata
            .modified()
            .map(format_modified_at)
            .unwrap_or_else(|_| "0".to_string()),
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
    Ok(entries)
}

// Builds the Axum router with all file-system endpoints wired to shared state.
fn build_app(shared_state: Arc<AppState>) -> Router {
    Router::new()
        .route("/list/", get(list_root))
        .route("/list/*path", get(list_path))
        .route(
            "/files/*path",
            get(get_file).put(write_file).delete(delete_path),
        )
        .route("/mkdir/*path", post(make_directory))
        .route("/rename", post(rename_entry))
        .with_state(shared_state)
}

// Reads the storage root from the first CLI argument, then env, then a default.
fn configured_storage_root() -> PathBuf {
    env::args()
        .nth(1)
        .or_else(|| env::var("REMOTE_FS_ROOT").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_ROOT))
}

// Handler for `POST /mkdir/*path`: creates a directory on disk.
async fn make_directory(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let directory_path = state.resolve_non_root_path(&path)?;
    println!("Server: Creating directory at /{}", path.trim_matches('/'));

    fs::create_dir(&directory_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Parent directory not found"))?;

    Ok((
        StatusCode::CREATED,
        format!("Directory {} created", path.trim_matches('/')),
    ))
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
    let stream = ReaderStream::new(file.take(response_size));

    Ok((StatusCode::OK, Body::from_stream(stream)).into_response())
}

// Handler for `PUT /files/*path`: streams the request body to disk at the requested offset.
async fn write_file(
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Body,
) -> Result<impl IntoResponse, StorageError> {
    let file_path = state.resolve_non_root_path(&path)?;
    let offset = parse_optional_u64_header(&headers, "X-File-Offset")?.unwrap_or(0);
    let truncate_size = parse_optional_u64_header(&headers, "X-File-Truncate")?;

    println!(
        "Server: Receiving streamed bytes for /{} (offset: {})",
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
        .read(true)
        .write(true)
        .open(&file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not open file"))?;

    if let Some(size) = truncate_size {
        file.set_len(size)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not resize file"))?;
        return Ok(StatusCode::OK);
    }

    let mut wrote_anything = false;

    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| StorageError::from_io(error, "Could not seek file"))?;

    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk.map_err(|_| StorageError::RequestBody("Could not read request body"))?;
        wrote_anything = true;
        file.write_all(&chunk)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not write file"))?;
    }

    if offset == 0 && !wrote_anything {
        file.set_len(0)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not truncate file"))?;
        return Ok(StatusCode::OK);
    }

    file.flush()
        .await
        .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;

    Ok(StatusCode::OK)
}

// Handler for `DELETE /files/*path`: removes a file or a full directory tree.
async fn delete_path(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    let metadata = fs::metadata(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    if metadata.is_dir() {
        fs::remove_dir_all(&target_path)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not remove directory"))?;
    } else {
        fs::remove_file(&target_path)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not remove file"))?;
    }

    Ok(StatusCode::NO_CONTENT)
}

// Handler for `POST /rename`: moves/renames files or directory trees.
async fn rename_entry(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RenameRequest>,
) -> Result<impl IntoResponse, StorageError> {
    let from_path = state.resolve_non_root_path(&payload.from)?;
    let to_path = state.resolve_non_root_path(&payload.to)?;
    let from_metadata = fs::metadata(&from_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Source path not found"))?;

    if from_metadata.is_dir() && to_path.starts_with(&from_path) {
        return Err(StorageError::BadRequest(
            "Cannot move a directory inside itself",
        ));
    }

    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not create parent directory"))?;
    }

    if let Ok(to_metadata) = fs::metadata(&to_path).await {
        if to_metadata.is_dir() {
            fs::remove_dir_all(&to_path)
                .await
                .map_err(|error| StorageError::from_io(error, "Could not replace directory"))?;
        } else {
            fs::remove_file(&to_path)
                .await
                .map_err(|error| StorageError::from_io(error, "Could not replace file"))?;
        }
    }

    fs::rename(&from_path, &to_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not rename path"))?;

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

#[tokio::main]
async fn main() {
    let storage_root = configured_storage_root();
    std::fs::create_dir_all(&storage_root).expect("Failed to create storage root");
    let storage_root =
        std::fs::canonicalize(&storage_root).expect("Failed to resolve storage root");

    let app = build_app(Arc::new(AppState::new(storage_root.clone())));
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server storage root: {}", storage_root.display());
    println!("Server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Method, Request, StatusCode},
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TestRoot {
        path: PathBuf,
    }

    impl TestRoot {
        fn new(name: &str) -> Self {
            let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = env::temp_dir().join(format!(
                "remote-fs-server-test-{}-{}-{}",
                name,
                std::process::id(),
                counter
            ));

            std::fs::create_dir_all(&path).unwrap();
            TestRoot { path }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn app_for_root(root: PathBuf) -> Router {
        build_app(Arc::new(AppState::new(root)))
    }

    #[tokio::test]
    async fn test_file_chunked_upload() {
        // 1. Start with an empty disk-backed server root.
        let root = TestRoot::new("chunked-upload");
        let app = app_for_root(root.path());

        // 2. Upload the first chunk at offset 0.
        let request_one = Request::builder()
            .method(Method::PUT)
            .uri("/files/test_upload.txt")
            .header("X-File-Offset", "0")
            .body(Body::from("Hello "))
            .unwrap();

        let response_one = app.clone().oneshot(request_one).await.unwrap();
        assert_eq!(response_one.status(), StatusCode::OK);

        // 3. Upload the second chunk after "Hello ".
        let request_two = Request::builder()
            .method(Method::PUT)
            .uri("/files/test_upload.txt")
            .header("X-File-Offset", "6")
            .body(Body::from("World!"))
            .unwrap();

        let response_two = app.oneshot(request_two).await.unwrap();
        assert_eq!(response_two.status(), StatusCode::OK);

        // 4. Check that both chunks were stitched together on disk.
        let saved_file = std::fs::read(root.path.join("test_upload.txt")).unwrap();
        assert_eq!(saved_file, b"Hello World!");
    }

    #[tokio::test]
    async fn test_large_upload_exceeds_default_body_limit() {
        // 1. Build a body larger than Axum's default buffered-body limit.
        let root = TestRoot::new("large-upload");
        let app = app_for_root(root.path());
        let data = vec![b'x'; 3 * 1024 * 1024];

        // 2. Upload it in one HTTP request; the server streams it to disk.
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/files/large.bin")
            .body(Body::from(data.clone()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 3. Verify the whole body landed on disk.
        assert_eq!(std::fs::read(root.path.join("large.bin")).unwrap(), data);
    }

    #[tokio::test]
    async fn test_range_read_returns_only_requested_bytes() {
        // 1. Seed a file directly on disk.
        let root = TestRoot::new("range-read");
        std::fs::write(root.path.join("letters.txt"), b"abcdefghijklmnopqrstuvwxyz").unwrap();

        // 2. Ask the API for only a small byte range.
        let app = app_for_root(root.path());
        let request = Request::builder()
            .method(Method::GET)
            .uri("/files/letters.txt")
            .header("X-File-Offset", "5")
            .header("X-File-Size", "4")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 3. Verify only that slice was returned.
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"fghi");
    }

    #[tokio::test]
    async fn test_resize_file_sets_file_length() {
        // 1. Seed a small file directly on disk.
        let root = TestRoot::new("resize");
        let app = app_for_root(root.path());
        std::fs::write(root.path.join("resize.bin"), b"hello").unwrap();

        // 2. Ask the API to resize it without sending a data body.
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/files/resize.bin")
            .header("X-File-Truncate", "1048576")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 3. Verify the file length changed on disk.
        assert_eq!(
            std::fs::metadata(root.path.join("resize.bin"))
                .unwrap()
                .len(),
            1_048_576
        );
    }

    #[tokio::test]
    async fn test_directory_listing_reflects_writes_and_mkdir() {
        // 1. Start with an empty disk-backed server root.
        let root = TestRoot::new("listing");
        let app = app_for_root(root.path());

        // 2. Create a directory through the API.
        let mkdir = Request::builder()
            .method(Method::POST)
            .uri("/mkdir/docs")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(mkdir).await.unwrap().status(),
            StatusCode::CREATED
        );

        // 3. Write a file inside the new directory.
        let write = Request::builder()
            .method(Method::PUT)
            .uri("/files/docs/readme.txt")
            .body(Body::from("hello"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(write).await.unwrap().status(),
            StatusCode::OK
        );

        // 4. List the directory and verify the new file appears.
        let list = Request::builder()
            .method(Method::GET)
            .uri("/list/docs")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(list).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let entries: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries[0]["name"], "readme.txt");
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[0]["size"], 5);
    }

    #[tokio::test]
    async fn test_delete_removes_file_and_directory_tree() {
        // 1. Seed a small directory tree directly on disk.
        let root = TestRoot::new("delete");
        std::fs::create_dir_all(root.path.join("docs/archive")).unwrap();
        std::fs::write(root.path.join("docs/archive/old.txt"), b"old").unwrap();

        // 2. Delete the tree through the API.
        let app = app_for_root(root.path());
        let request = Request::builder()
            .method(Method::DELETE)
            .uri("/files/docs")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // 3. Verify both the directory and its nested file are gone.
        assert!(!root.path.join("docs").exists());
        assert!(!root.path.join("docs/archive/old.txt").exists());
    }

    #[tokio::test]
    async fn test_rename_moves_directory_tree() {
        // 1. Seed a directory tree directly on disk.
        let root = TestRoot::new("rename");
        std::fs::create_dir_all(root.path.join("docs/archive")).unwrap();
        std::fs::write(root.path.join("docs/archive/old.txt"), b"old").unwrap();

        // 2. Rename the tree through the API.
        let app = app_for_root(root.path());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/rename")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "from": "docs", "to": "renamed/docs" }).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 3. Verify old paths disappeared and new paths contain the same data.
        assert!(!root.path.join("docs").exists());
        assert!(root.path.join("renamed/docs/archive").is_dir());
        assert_eq!(
            std::fs::read(root.path.join("renamed/docs/archive/old.txt")).unwrap(),
            b"old"
        );
    }

    #[tokio::test]
    async fn test_rename_rejects_moving_directory_inside_itself() {
        // 1. Seed a directory tree directly on disk.
        let root = TestRoot::new("rename-inside-self");
        std::fs::create_dir_all(root.path.join("docs/archive")).unwrap();

        // 2. Try to move the directory inside its own subtree.
        let app = app_for_root(root.path());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/rename")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "from": "docs", "to": "docs/archive/docs" }).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // 3. Verify the original tree is still in place.
        assert!(root.path.join("docs/archive").is_dir());
    }

    #[tokio::test]
    async fn test_written_file_survives_new_router_with_same_root() {
        // 1. Write a file through one router instance.
        let root = TestRoot::new("persistence");
        let app = app_for_root(root.path());
        let write = Request::builder()
            .method(Method::PUT)
            .uri("/files/persistent.txt")
            .body(Body::from("still here"))
            .unwrap();
        assert_eq!(app.oneshot(write).await.unwrap().status(), StatusCode::OK);

        // 2. Build a fresh router and read the same file from disk.
        let app = app_for_root(root.path());
        let read = Request::builder()
            .method(Method::GET)
            .uri("/files/persistent.txt")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(read).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"still here");
    }

    #[test]
    fn test_path_traversal_is_rejected() {
        // 1. Build state with any root; validation should reject before disk access.
        let root = TestRoot::new("path-validation");
        let state = AppState::new(root.path());

        // 2. Ensure parent-directory components cannot escape the storage root.
        assert!(state.resolve_path("../Cargo.toml").is_err());
        assert!(state.resolve_path("docs/../../Cargo.toml").is_err());
    }
}
