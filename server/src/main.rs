// This is a server for a remote file system.
// It provides a basic API for listing directory contents.
//
// To run the server, navigate to the `server` directory
// and run the command: `cargo run`
//
// The server will listen on `0.0.0.0:3000`.

use axum::{body::Bytes,
           extract::{Path, State},
           http::{HeaderMap, StatusCode},
           response::IntoResponse,
           routing::{get, post, put},
           Json, Router,
};
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

struct AppState {
    file_contents: Mutex<HashMap<String, Vec<u8>>>,
}

// Handler for the `POST /mkdir/*path` endpoint.
async fn make_directory(Path(path): Path<String>) -> impl IntoResponse {
    println!("Mock Server: Creating directory at /{}", path);

    // We just pretend it succeeded and return a 201 Created status
    (StatusCode::CREATED, format!("Directory {} created", path))
}

// Handler for the `GET /files/*path` endpoint.
async fn get_file(Path(path): Path<String>, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let contents = state.file_contents.lock().unwrap();

    if let Some(data) = contents.get(&path) {
    (StatusCode::OK, data.clone()).into_response()
    } else {
    (StatusCode::NOT_FOUND, "File not found".to_string()).into_response()
    }
}

//Handler per `PUT /files/*path`
async fn write_file(Path(path): Path<String>, headers: HeaderMap, State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    let offset: usize = headers
        .get("X-File-Offset")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    println!("Mock Server: Received {} byte for /{} (offset: {})", body.len(), path, offset);

    let mut contents = state.file_contents.lock().unwrap();

    let file_data = contents.entry(path).or_insert_with(Vec::new);

    if file_data.len() < offset + body.len() {
        file_data.resize(offset + body.len(), 0);
    }

    file_data[offset..offset + body.len()].copy_from_slice(&body);

    StatusCode::OK
}

/// Represents a directory entry (file or directory).
#[derive(Serialize)]
struct DirectoryEntry {
    name: String,
    #[serde(rename = "type")]
    type_: String, // Renamed to avoid conflict with Rust keyword
    size: u64, // Size in bytes for files, 0 for directories
    modified_at: String, // ISO 8601 timestamp
}

// Handler for the `GET /list/` endpoint.
// Returns a list of mock directory entries for the root directory.
//
// Example JSON response:
// [
//   {
//     "name": "Documents",
//     "type": "directory",
//     "size": 0,
//     "modified_at": "2024-05-22T10:00:00Z"
//   },
//   {
//     "name": "image.jpg",
//     "type": "file",
//     "size": 102400,
//     "modified_at": "2024-05-22T11:30:00Z"
//   },
//   {
//     "name": "folder1",
//     "type": "directory",
//     "size": 0,
//     "modified_at": "2024-05-23T12:00:00Z"
//   }
// ]
async fn list_root() -> Json<Vec<DirectoryEntry>> {
    let entries = vec![
        DirectoryEntry {
            name: "Documents".to_string(),
            type_: "directory".to_string(),
            size: 0,
            modified_at: "2024-05-22T10:00:00Z".to_string(),
        },
        DirectoryEntry {
            name: "image.jpg".to_string(),
            type_: "file".to_string(),
            size: 102400,
            modified_at: "2024-05-22T11:30:00Z".to_string(),
        },
        DirectoryEntry {
            name: "folder1".to_string(),
            type_: "directory".to_string(),
            size: 0,
            modified_at: "2024-05-23T12:00:00Z".to_string(),
        },
    ];
    Json(entries)
}

// Handler for the `GET /list/*path` endpoint.
// Returns a list of mock directory entries for the specified path.
//
// Specifically mocked path: `GET /list/folder1/`
// Example JSON response for `/list/folder1/`:
// [
//   {
//     "name": "file1.txt",
//     "type": "file",
//     "size": 1024,
//     "modified_at": "2024-05-23T12:05:00Z"
//   }
// ]
//
// Other paths under `/list/` (e.g., `/list/nonexistent_folder/`) will return an empty JSON array `[]`.
async fn list_path(Path(path): Path<String>) -> Json<Vec<DirectoryEntry>> {
    if path == "folder1" {
        let entries = vec![DirectoryEntry {
            name: "file1.txt".to_string(),
            type_: "file".to_string(),
            size: 22,
            modified_at: "2024-05-23T12:05:00Z".to_string(),
        }];
        Json(entries)
    } else {
        Json(vec![])
    }
}

#[tokio::main]
async fn main() {
    let mut initial_contents = HashMap::new();
    initial_contents.insert("folder1/file1.txt".to_string(), b"Hello from file1.txt!\n".to_vec());
    initial_contents.insert("image.jpg".to_string(), b"Fake image data...".to_vec());

    let shared_state = Arc::new(AppState {
        file_contents: Mutex::new(initial_contents),
    });
    // Define Axum application router.
    let app = Router::new()
        .route("/list/", get(list_root)) // Route for listing the root directory
        .route("/list/*path", get(list_path)) // Route for listing a specific path
        .route("/files/*path", get(get_file)) // Route for getting file content
        .route("/files/*path", put(write_file)) // Route for uploading and saving file contents to the server
        .route("/mkdir/*path", post(make_directory)) // Route for creating a directory
        .with_state(shared_state);
    // Define the server address.
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Mock server listening on {}", addr);

    // Start the Axum server using the new API.
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}



#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, Method, StatusCode},
    };
    use tower::ServiceExt; // Required to use `.oneshot()` for testing Routers

    #[tokio::test]
    async fn test_file_chunked_upload() {
        // 1. Setup a fresh, empty server state
        let shared_state = Arc::new(AppState {
            file_contents: Mutex::new(HashMap::new()),
        });

        // 2. Build the router with just the PUT endpoint
        let app = Router::new()
            .route("/files/*path", axum::routing::put(write_file))
            .with_state(shared_state.clone());

        // 3. Simulating the FUSE client sending the FIRST chunk (Offset 0)
        let request_one = Request::builder()
            .method(Method::PUT)
            .uri("/files/test_upload.txt")
            .header("X-File-Offset", "0")
            .body(Body::from("Hello "))
            .unwrap();

        // Send the first request
        let response_one = app.clone().oneshot(request_one).await.unwrap();
        assert_eq!(response_one.status(), StatusCode::OK);

        // 4. Simulating the FUSE client sending the SECOND chunk (Offset 6)
        let request_two = Request::builder()
            .method(Method::PUT)
            .uri("/files/test_upload.txt")
            .header("X-File-Offset", "6") // Start writing after "Hello "
            .body(Body::from("World!"))
            .unwrap();

        // Send the second request
        let response_two = app.oneshot(request_two).await.unwrap();
        assert_eq!(response_two.status(), StatusCode::OK);

        // 5. Check the server's RAM directly
        let contents = shared_state.file_contents.lock().unwrap();
        let saved_file = contents.get("test_upload.txt").expect("File was not created in memory!");

        // Assert that the two chunks were stitched together perfectly
        assert_eq!(saved_file, b"Hello World!");
    }
}
