use crate::{
    error::StorageError,
    metadata::{
        apply_metadata_headers, directory_entry_from_metadata, entry_metadata_for_path,
        parse_optional_u64_header,
    },
    transaction::create_file_atomically,
    AppState, INTERNAL_DIR_NAME,
};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header::IF_NONE_MATCH, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use remote_fs_protocol::{headers, DirectoryEntry, RemoteMetadata, RenameRequest};
use std::{io, io::SeekFrom, sync::Arc};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;

const TRANSFER_BUFFER_SIZE: usize = 4 * 1024 * 1024;

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
        if path.trim_matches('/').is_empty() && name == INTERNAL_DIR_NAME {
            continue;
        }
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

pub(crate) async fn make_directory(
    AxumPath(path): AxumPath<String>,
    headers_map: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let directory_path = state.resolve_non_root_path(&path)?;

    fs::create_dir(&directory_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Parent directory not found"))?;

    apply_metadata_headers(&directory_path, &headers_map).await?;
    let metadata = entry_metadata_for_path(&directory_path).await?;
    log::info!("Created directory /{}", path.trim_matches('/'));

    Ok((StatusCode::CREATED, Json(metadata)))
}

pub(crate) async fn get_file(
    AxumPath(path): AxumPath<String>,
    headers_map: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Response, StorageError> {
    let file_path = state.resolve_non_root_path(&path)?;
    let metadata = fs::metadata(&file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "File not found"))?;

    if !metadata.is_file() {
        return Err(StorageError::BadRequest("Path is not a file"));
    }

    let offset = parse_optional_u64_header(&headers_map, headers::FILE_OFFSET)?.unwrap_or(0);
    let requested_size = parse_optional_u64_header(&headers_map, headers::FILE_SIZE)?;

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

pub(crate) async fn write_file(
    AxumPath(path): AxumPath<String>,
    headers_map: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Body,
) -> Result<Response, StorageError> {
    let file_path = state.resolve_non_root_path(&path)?;
    let offset = parse_optional_u64_header(&headers_map, headers::FILE_OFFSET)?.unwrap_or(0);
    let truncate_size = parse_optional_u64_header(&headers_map, headers::FILE_TRUNCATE)?;

    if headers_map
        .get(IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == b"*")
    {
        if offset != 0 || truncate_size.is_some() {
            return Err(StorageError::BadRequest(
                "Conditional creation does not support offsets or truncation",
            ));
        }
        return create_file_atomically(&path, &file_path, &headers_map, &state, body).await;
    }

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
        apply_metadata_headers(&file_path, &headers_map).await?;
        file.sync_all()
            .await
            .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
        drop(file);
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
        apply_metadata_headers(&file_path, &headers_map).await?;
        file.sync_all()
            .await
            .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
        drop(file);
        let metadata = entry_metadata_for_path(&file_path).await?;
        log::info!("Created or truncated /{}", path.trim_matches('/'));
        return Ok((StatusCode::OK, Json(metadata)).into_response());
    }

    apply_metadata_headers(&file_path, &headers_map).await?;
    file.sync_all()
        .await
        .map_err(|error| StorageError::from_io(error, "Could not flush file"))?;
    drop(file);
    let metadata = entry_metadata_for_path(&file_path).await?;
    log::info!(
        "Wrote /{} (offset: {}, bytes: {})",
        path.trim_matches('/'),
        offset,
        bytes_written
    );

    Ok((StatusCode::OK, Json(metadata)).into_response())
}

pub(crate) async fn get_metadata(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<RemoteMetadata>, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    let metadata = entry_metadata_for_path(&target_path).await?;
    log::debug!("Read metadata for /{}", path.trim_matches('/'));
    Ok(Json(metadata))
}

pub(crate) async fn update_metadata(
    AxumPath(path): AxumPath<String>,
    headers_map: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StorageError> {
    let target_path = state.resolve_non_root_path(&path)?;
    fs::metadata(&target_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    apply_metadata_headers(&target_path, &headers_map).await?;
    let metadata = entry_metadata_for_path(&target_path).await?;
    log::info!("Updated metadata for /{}", path.trim_matches('/'));

    Ok((StatusCode::OK, Json(metadata)))
}

pub(crate) async fn delete_path(
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

pub(crate) async fn delete_directory(
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

pub(crate) async fn rename_entry(
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

pub(crate) async fn list_root(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DirectoryEntry>>, StorageError> {
    Ok(Json(list_entries(&state, "").await?))
}

pub(crate) async fn list_path(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DirectoryEntry>>, StorageError> {
    Ok(Json(list_entries(&state, &path).await?))
}
