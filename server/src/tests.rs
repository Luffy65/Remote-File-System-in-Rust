use super::*;
use axum::{
    body::{to_bytes, Body, Bytes},
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
    build_app(Arc::new(AppState::with_auth(root, None)))
}

fn app_for_root_with_token(root: PathBuf, token: &str) -> Router {
    build_app(Arc::new(AppState::with_auth(root, Some(token.to_string()))))
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
async fn test_upload_over_100_mb_streams_to_disk() {
    // Stream a body larger than 100 MiB without keeping a second full copy for verification.
    let root = TestRoot::new("over-100-mb-upload");
    let app = app_for_root(root.path());
    const CHUNK_SIZE: usize = 1024 * 1024;
    const CHUNK_COUNT: usize = 101;
    const EXPECTED_SIZE: u64 = (CHUNK_SIZE * CHUNK_COUNT) as u64;

    let chunk = Bytes::from(vec![b'x'; CHUNK_SIZE]);
    let body = Body::from_stream(futures_util::stream::iter(
        (0..CHUNK_COUNT).map(move |_| Ok::<Bytes, std::io::Error>(chunk.clone())),
    ));

    let request = Request::builder()
        .method(Method::PUT)
        .uri("/files/over-100-mb.bin")
        .body(body)
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        std::fs::metadata(root.path.join("over-100-mb.bin"))
            .unwrap()
            .len(),
        EXPECTED_SIZE
    );
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
async fn test_write_returns_real_metadata() {
    // 1. Write a file through the API.
    let root = TestRoot::new("write-metadata");
    let app = app_for_root(root.path());
    let request = Request::builder()
        .method(Method::PUT)
        .uri("/files/meta.txt")
        .body(Body::from("metadata"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 2. Verify the server returns metadata from the saved file.
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(metadata["type"], "file");
    assert_eq!(metadata["size"], 8);
    assert!(
        metadata["modified_at"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert!(metadata["mode"].as_u64().is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn test_metadata_patch_updates_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    // 1. Seed a file and update its permissions through the metadata endpoint.
    let root = TestRoot::new("metadata-mode");
    std::fs::write(root.path.join("mode.txt"), b"mode").unwrap();
    let app = app_for_root(root.path());
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/metadata/mode.txt")
        .header("X-File-Mode", "600")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 2. Verify both the JSON response and the real file mode.
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(metadata["mode"], 0o600);
    assert_eq!(
        std::fs::metadata(root.path.join("mode.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
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
async fn test_delete_removes_file_and_empty_directory() {
    let root = TestRoot::new("delete");
    std::fs::write(root.path.join("old.txt"), b"old").unwrap();
    std::fs::create_dir(root.path.join("empty")).unwrap();

    let app = app_for_root(root.path());
    let delete_file = Request::builder()
        .method(Method::DELETE)
        .uri("/files/old.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(delete_file).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let delete_dir = Request::builder()
        .method(Method::DELETE)
        .uri("/directories/empty")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(delete_dir).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    assert!(!root.path.join("old.txt").exists());
    assert!(!root.path.join("empty").exists());
}

#[tokio::test]
async fn test_delete_directory_rejects_non_empty_directory() {
    let root = TestRoot::new("delete-non-empty");
    std::fs::create_dir(root.path.join("docs")).unwrap();
    std::fs::write(root.path.join("docs/old.txt"), b"old").unwrap();

    let app = app_for_root(root.path());
    let request = Request::builder()
        .method(Method::DELETE)
        .uri("/directories/docs")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    assert_eq!(
        std::fs::read(root.path.join("docs/old.txt")).unwrap(),
        b"old"
    );
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
async fn test_rename_does_not_delete_existing_non_empty_directory() {
    let root = TestRoot::new("rename-existing-dir");
    std::fs::create_dir(root.path.join("source")).unwrap();
    std::fs::create_dir_all(root.path.join("target/archive")).unwrap();
    std::fs::write(root.path.join("target/archive/old.txt"), b"old").unwrap();

    let app = app_for_root(root.path());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/rename")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "from": "source", "to": "target" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert!(response.status().is_client_error());

    assert!(root.path.join("source").is_dir());
    assert_eq!(
        std::fs::read(root.path.join("target/archive/old.txt")).unwrap(),
        b"old"
    );
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
    let state = AppState::with_auth(root.path(), None);

    // 2. Ensure parent-directory components cannot escape the storage root.
    assert!(state.resolve_path("../Cargo.toml").is_err());
    assert!(state.resolve_path("docs/../../Cargo.toml").is_err());
}

#[test]
fn test_link_components_are_rejected() {
    let root = TestRoot::new("link-root");
    let outside = TestRoot::new("link-outside");
    std::fs::write(outside.path.join("secret.txt"), b"secret").unwrap();
    let link = root.path.join("escape");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside.path, &link).unwrap();

    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside.path)
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test junction");
    }

    let state = AppState::with_auth(root.path(), None);
    assert!(matches!(
        state.resolve_path("escape/secret.txt"),
        Err(StorageError::Forbidden(_))
    ));

    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
}

#[tokio::test]
async fn test_bearer_token_authentication() {
    let root = TestRoot::new("authentication");
    let app = app_for_root_with_token(root.path(), "correct-token");

    let missing = Request::builder()
        .uri("/list/")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(missing).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let incorrect = Request::builder()
        .uri("/list/")
        .header("authorization", "Bearer wrong-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(incorrect).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let authorized = Request::builder()
        .uri("/list/")
        .header("authorization", "Bearer correct-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(authorized).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_percent_encoded_reserved_filename_round_trips() {
    let root = TestRoot::new("encoded-filename");
    let app = app_for_root(root.path());
    let write = Request::builder()
        .method(Method::PUT)
        .uri("/files/hash%23%20percent%25.txt")
        .body(Body::from("encoded"))
        .unwrap();

    assert_eq!(
        app.clone().oneshot(write).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        std::fs::read(root.path.join("hash# percent%.txt")).unwrap(),
        b"encoded"
    );

    let read = Request::builder()
        .uri("/files/hash%23%20percent%25.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(read).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..],
        b"encoded"
    );
}

#[tokio::test]
async fn test_rename_respects_replace_if_exists() {
    let root = TestRoot::new("rename-replace-semantics");
    std::fs::write(root.path.join("source.txt"), b"source").unwrap();
    std::fs::write(root.path.join("destination.txt"), b"destination").unwrap();
    let app = app_for_root(root.path());

    let reject_replace = Request::builder()
        .method(Method::POST)
        .uri("/rename")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "from": "source.txt",
                "to": "destination.txt",
                "replace_if_exists": false
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(reject_replace).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        std::fs::read(root.path.join("source.txt")).unwrap(),
        b"source"
    );
    assert_eq!(
        std::fs::read(root.path.join("destination.txt")).unwrap(),
        b"destination"
    );

    let allow_replace = Request::builder()
        .method(Method::POST)
        .uri("/rename")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "from": "source.txt",
                "to": "destination.txt",
                "replace_if_exists": true
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.oneshot(allow_replace).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(!root.path.join("source.txt").exists());
    assert_eq!(
        std::fs::read(root.path.join("destination.txt")).unwrap(),
        b"source"
    );
}

#[tokio::test]
async fn test_metadata_patch_updates_modification_time() {
    const REQUESTED_MTIME: u64 = 946_684_800;

    let root = TestRoot::new("metadata-mtime");
    std::fs::write(root.path.join("mtime.txt"), b"mtime").unwrap();
    let app = app_for_root(root.path());
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/metadata/mtime.txt")
        .header("X-File-Mtime", REQUESTED_MTIME.to_string())
        .body(Body::empty())
        .unwrap();

    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    let modified_at = std::fs::metadata(root.path.join("mtime.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(modified_at, REQUESTED_MTIME);
}

#[tokio::test]
async fn test_metadata_get_returns_one_path_without_listing_parent() {
    let root = TestRoot::new("metadata-get");
    std::fs::create_dir(root.path.join("docs")).unwrap();
    std::fs::write(root.path.join("docs/file.txt"), b"metadata").unwrap();
    std::fs::write(root.path.join("docs/unrelated.txt"), b"other").unwrap();
    let app = app_for_root(root.path());

    let request = Request::builder()
        .uri("/metadata/docs/file.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(metadata["type"], "file");
    assert_eq!(metadata["size"], 8);
    assert!(metadata.get("name").is_none());

    let missing = Request::builder()
        .uri("/metadata/docs/missing.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(missing).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
