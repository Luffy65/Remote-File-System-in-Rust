// This is a server for a remote file system.
// It provides a basic REST API for listing, reading, writing, creating,
// deleting, and renaming files/directories in an in-memory mock store.
//
// To run the server, navigate to the `server` directory
// and run the command: `cargo run`
//
// The server will listen on `0.0.0.0:3000`.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const MOCK_TIMESTAMP: &str = "2024-05-23T12:00:00Z";

struct AppState {
    file_contents: Mutex<HashMap<String, Vec<u8>>>,
    directories: Mutex<HashSet<String>>,
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

// Converts API paths to the internal format: no leading/trailing slashes.
fn normalize_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

// Returns the parent directory path, or "" when the parent is root.
fn parent_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

// Adds every parent segment of a path to the in-memory directory set.
fn ensure_parent_directories(directories: &mut HashSet<String>, path: &str) {
    let mut current = String::new();

    for part in path.split('/').filter(|part| !part.is_empty()) {
        if current.is_empty() {
            current.push_str(part);
        } else {
            current.push('/');
            current.push_str(part);
        }
        directories.insert(current.clone());
    }
}

// Returns child's basename only when it is directly inside parent.
fn is_direct_child(parent: &str, child: &str) -> Option<String> {
    if parent.is_empty() {
        if child.is_empty() || child.contains('/') {
            None
        } else {
            Some(child.to_string())
        }
    } else {
        let prefix = format!("{}/", parent);
        let child_tail = child.strip_prefix(&prefix)?;

        if child_tail.is_empty() || child_tail.contains('/') {
            None
        } else {
            Some(child_tail.to_string())
        }
    }
}

// Builds a directory listing from the current in-memory files and directories.
fn list_entries(state: &AppState, path: &str) -> Vec<DirectoryEntry> {
    let normalized_path = normalize_path(path);
    let directories = state.directories.lock().unwrap();
    let contents = state.file_contents.lock().unwrap();
    let mut entries = Vec::new();

    for directory in directories.iter() {
        if directory == &normalized_path {
            continue;
        }

        if let Some(name) = is_direct_child(&normalized_path, directory) {
            entries.push(DirectoryEntry {
                name,
                type_: "directory".to_string(),
                size: 0,
                modified_at: MOCK_TIMESTAMP.to_string(),
            });
        }
    }

    for (file_path, data) in contents.iter() {
        if let Some(name) = is_direct_child(&normalized_path, file_path) {
            entries.push(DirectoryEntry {
                name,
                type_: "file".to_string(),
                size: data.len() as u64,
                modified_at: MOCK_TIMESTAMP.to_string(),
            });
        }
    }

    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

// Maps a path from an old prefix to a new prefix during directory renames.
fn rename_path(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        Some(to.to_string())
    } else {
        path.strip_prefix(&format!("{}/", from))
            .map(|suffix| format!("{}/{}", to, suffix))
    }
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

// Creates the initial mock filesystem contents used at server startup.
fn initial_state() -> Arc<AppState> {
    let mut initial_contents = HashMap::new();
    initial_contents.insert(
        "folder1/file1.txt".to_string(),
        b"Hello from file1.txt!\n".to_vec(),
    );
    initial_contents.insert("image.jpg".to_string(), b"Fake image data...".to_vec());

    let mut directories = HashSet::new();
    directories.insert(String::new());
    directories.insert("Documents".to_string());
    directories.insert("folder1".to_string());

    Arc::new(AppState {
        file_contents: Mutex::new(initial_contents),
        directories: Mutex::new(directories),
    })
}

// Handler for `POST /mkdir/*path`: records a new directory in memory.
async fn make_directory(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let normalized_path = normalize_path(&path);
    println!("Mock Server: Creating directory at /{}", normalized_path);

    if normalized_path.is_empty() {
        return (StatusCode::BAD_REQUEST, "Directory path cannot be empty").into_response();
    }

    let parent = parent_path(&normalized_path);
    let mut directories = state.directories.lock().unwrap();

    if !directories.contains(&parent) {
        return (StatusCode::NOT_FOUND, "Parent directory not found").into_response();
    }

    directories.insert(normalized_path.clone());
    (
        StatusCode::CREATED,
        format!("Directory {} created", normalized_path),
    )
        .into_response()
}

// Handler for `GET /files/*path`: returns the stored bytes for a file.
async fn get_file(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let normalized_path = normalize_path(&path);
    let contents = state.file_contents.lock().unwrap();

    if let Some(data) = contents.get(&normalized_path) {
        (StatusCode::OK, data.clone()).into_response()
    } else {
        (StatusCode::NOT_FOUND, "File not found".to_string()).into_response()
    }
}

// Handler for `PUT /files/*path`: writes a byte chunk at the requested offset.
async fn write_file(
    Path(path): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> impl IntoResponse {
    let normalized_path = normalize_path(&path);
    let offset: usize = headers
        .get("X-File-Offset")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    println!(
        "Mock Server: Received {} byte for /{} (offset: {})",
        body.len(),
        normalized_path,
        offset
    );

    {
        let mut directories = state.directories.lock().unwrap();
        ensure_parent_directories(&mut directories, &parent_path(&normalized_path));
    }

    let mut contents = state.file_contents.lock().unwrap();
    let file_data = contents.entry(normalized_path).or_insert_with(Vec::new);

    if file_data.len() < offset + body.len() {
        file_data.resize(offset + body.len(), 0);
    }

    file_data[offset..offset + body.len()].copy_from_slice(&body);

    StatusCode::OK.into_response()
}

// Handler for `DELETE /files/*path`: removes a file or a full directory tree.
async fn delete_path(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let normalized_path = normalize_path(&path);

    if normalized_path.is_empty() {
        return (StatusCode::BAD_REQUEST, "Cannot delete root").into_response();
    }

    let mut directories = state.directories.lock().unwrap();
    let mut contents = state.file_contents.lock().unwrap();
    let mut deleted = contents.remove(&normalized_path).is_some();

    if directories.remove(&normalized_path) {
        deleted = true;
        let prefix = format!("{}/", normalized_path);
        directories.retain(|dir| !dir.starts_with(&prefix));
        contents.retain(|file_path, _| !file_path.starts_with(&prefix));
    }

    if deleted {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "Path not found").into_response()
    }
}

// Handler for `POST /rename`: moves/renames files or directory trees.
async fn rename_entry(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RenameRequest>,
) -> impl IntoResponse {
    let from = normalize_path(&payload.from);
    let to = normalize_path(&payload.to);

    if from.is_empty() || to.is_empty() {
        return (StatusCode::BAD_REQUEST, "Rename paths cannot be empty").into_response();
    }

    {
        let mut directories = state.directories.lock().unwrap();
        ensure_parent_directories(&mut directories, &parent_path(&to));
    }

    let mut directories = state.directories.lock().unwrap();
    let mut contents = state.file_contents.lock().unwrap();
    let mut renamed = false;

    if !contents.contains_key(&from) && !directories.contains(&from) {
        return (StatusCode::NOT_FOUND, "Source path not found").into_response();
    }

    let to_prefix = format!("{}/", to);
    contents.remove(&to);
    contents.retain(|path, _| !path.starts_with(&to_prefix));
    directories.remove(&to);
    directories.retain(|dir| !dir.starts_with(&to_prefix));

    if let Some(data) = contents.remove(&from) {
        contents.insert(to.clone(), data);
        renamed = true;
    }

    if directories.contains(&from) {
        let old_directories: Vec<String> = directories.iter().cloned().collect();
        let old_files: Vec<(String, Vec<u8>)> = contents
            .iter()
            .map(|(path, data)| (path.clone(), data.clone()))
            .collect();

        directories.retain(|dir| rename_path(dir, &from, &to).is_none());
        for dir in old_directories {
            if let Some(new_path) = rename_path(&dir, &from, &to) {
                directories.insert(new_path);
            }
        }

        contents.retain(|path, _| rename_path(path, &from, &to).is_none());
        for (path, data) in old_files {
            if let Some(new_path) = rename_path(&path, &from, &to) {
                contents.insert(new_path, data);
            }
        }

        renamed = true;
    }

    debug_assert!(renamed);
    StatusCode::OK.into_response()
}

// Handler for `GET /list/`: lists the root directory.
async fn list_root(State(state): State<Arc<AppState>>) -> Json<Vec<DirectoryEntry>> {
    Json(list_entries(&state, ""))
}

// Handler for `GET /list/*path`: lists a specific directory.
async fn list_path(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<Vec<DirectoryEntry>> {
    Json(list_entries(&state, &path))
}

#[tokio::main]
async fn main() {
    let app = build_app(initial_state());
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Mock server listening on {}", addr);

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
    use tower::ServiceExt;

    fn empty_state() -> Arc<AppState> {
        let mut directories = HashSet::new();
        directories.insert(String::new());

        Arc::new(AppState {
            file_contents: Mutex::new(HashMap::new()),
            directories: Mutex::new(directories),
        })
    }

    #[tokio::test]
    async fn test_file_chunked_upload() {
        // 1. Start with an empty mock server state.
        let shared_state = empty_state();
        let app = build_app(shared_state.clone());

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

        // 4. Check that both chunks were stitched together in memory.
        let contents = shared_state.file_contents.lock().unwrap();
        let saved_file = contents
            .get("test_upload.txt")
            .expect("File was not created in memory!");

        assert_eq!(saved_file, b"Hello World!");
    }

    #[tokio::test]
    async fn test_directory_listing_reflects_writes_and_mkdir() {
        // 1. Start with an empty mock server state.
        let app = build_app(empty_state());

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
        // 1. Seed a small directory tree directly in the mock state.
        let shared_state = empty_state();
        {
            let mut directories = shared_state.directories.lock().unwrap();
            directories.insert("docs".to_string());
            directories.insert("docs/archive".to_string());
        }
        shared_state
            .file_contents
            .lock()
            .unwrap()
            .insert("docs/archive/old.txt".to_string(), b"old".to_vec());

        // 2. Delete the tree through the API.
        let app = build_app(shared_state.clone());
        let request = Request::builder()
            .method(Method::DELETE)
            .uri("/files/docs")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // 3. Verify both the directory and its nested file are gone.
        assert!(!shared_state.directories.lock().unwrap().contains("docs"));
        assert!(!shared_state
            .file_contents
            .lock()
            .unwrap()
            .contains_key("docs/archive/old.txt"));
    }

    #[tokio::test]
    async fn test_rename_moves_directory_tree() {
        // 1. Seed a directory tree directly in the mock state.
        let shared_state = empty_state();
        {
            let mut directories = shared_state.directories.lock().unwrap();
            directories.insert("docs".to_string());
            directories.insert("docs/archive".to_string());
        }
        shared_state
            .file_contents
            .lock()
            .unwrap()
            .insert("docs/archive/old.txt".to_string(), b"old".to_vec());

        // 2. Rename the tree through the API.
        let app = build_app(shared_state.clone());
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
        assert!(!shared_state.directories.lock().unwrap().contains("docs"));
        assert!(shared_state
            .directories
            .lock()
            .unwrap()
            .contains("renamed/docs/archive"));
        assert_eq!(
            shared_state
                .file_contents
                .lock()
                .unwrap()
                .get("renamed/docs/archive/old.txt")
                .unwrap(),
            b"old"
        );
    }
}
