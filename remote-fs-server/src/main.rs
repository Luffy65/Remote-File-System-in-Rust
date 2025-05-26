// This is a mock server for a remote file system.
// It provides a basic API for listing directory contents.
//
// To run the server, navigate to the `remote-fs-server` directory
// and run the command: `cargo run`
//
// The server will listen on `0.0.0.0:3000`.

use axum::{extract::Path, routing::get, Json, Router};
use serde::Serialize;
use std::net::SocketAddr;

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
            size: 1024,
            modified_at: "2024-05-23T12:05:00Z".to_string(),
        }];
        Json(entries)
    } else {
        Json(vec![])
    }
}

#[tokio::main]
async fn main() {
    // Define the Axum application router.
    let app = Router::new()
        .route("/list/", get(list_root)) // Route for listing the root directory
        .route("/list/*path", get(list_path)); // Route for listing a specific path

    // Define the server address.
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Mock server listening on {}", addr);

    // Start the Axum server using the new API.
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
