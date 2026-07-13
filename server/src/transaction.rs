//! Atomic create-only commits.
//!
//! Invariant: a conditional create is never visible at its destination until
//! its complete body and metadata have been synced. A destination that already
//! exists is never overwritten.

use crate::{
    error::StorageError,
    metadata::{apply_metadata_headers, entry_metadata_for_path},
    AppState,
};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn sync_directory(path: PathBuf) -> Result<(), StorageError> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }

    #[cfg(not(windows))]
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(path);
        directory?.sync_all()
    })
    .await
    .map_err(|error| StorageError::Io(io::Error::other(error)))?
    .map_err(|error| StorageError::from_io(error, "Could not sync directory"))
}

pub(crate) async fn create_file_atomically(
    path: &str,
    file_path: &Path,
    headers: &HeaderMap,
    state: &AppState,
    body: Body,
) -> Result<Response, StorageError> {
    let transaction_id = TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let transaction_path = state.transaction_dir.join(format!(
        "upload-{}-{transaction_id}.tmp",
        std::process::id()
    ));

    let mut transaction_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&transaction_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not create transaction file"))?;

    let mut body_stream = body.into_data_stream();
    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                drop(transaction_file);
                let _ = fs::remove_file(&transaction_path).await;
                return Err(StorageError::RequestBody("Could not read request body"));
            }
        };
        if let Err(error) = transaction_file.write_all(&chunk).await {
            drop(transaction_file);
            let _ = fs::remove_file(&transaction_path).await;
            return Err(StorageError::from_io(
                error,
                "Could not write transaction file",
            ));
        }
    }

    if let Err(error) = transaction_file.sync_all().await {
        drop(transaction_file);
        let _ = fs::remove_file(&transaction_path).await;
        return Err(StorageError::from_io(
            error,
            "Could not sync transaction file",
        ));
    }
    if let Err(error) = apply_metadata_headers(&transaction_path, headers).await {
        drop(transaction_file);
        let _ = fs::remove_file(&transaction_path).await;
        return Err(error);
    }
    if let Err(error) = transaction_file.sync_all().await {
        drop(transaction_file);
        let _ = fs::remove_file(&transaction_path).await;
        return Err(StorageError::from_io(
            error,
            "Could not sync transaction metadata",
        ));
    }
    drop(transaction_file);

    let _mutation_guard = state.mutation_lock.lock().await;
    match fs::symlink_metadata(file_path).await {
        Ok(_) => {
            let _ = fs::remove_file(&transaction_path).await;
            return Err(StorageError::PreconditionFailed(
                "Destination path already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = fs::remove_file(&transaction_path).await;
            return Err(StorageError::from_io(
                error,
                "Could not inspect destination path",
            ));
        }
    }

    let parent = file_path
        .parent()
        .ok_or(StorageError::BadRequest("Path has no parent directory"))?;
    if let Err(error) = fs::create_dir_all(parent).await {
        let _ = fs::remove_file(&transaction_path).await;
        return Err(StorageError::from_io(
            error,
            "Could not create parent directory",
        ));
    }
    if let Err(error) = fs::hard_link(&transaction_path, file_path).await {
        let _ = fs::remove_file(&transaction_path).await;
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Err(StorageError::PreconditionFailed(
                "Destination path already exists",
            ));
        }
        return Err(StorageError::from_io(
            error,
            "Could not commit transaction file",
        ));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .await
        .map_err(|error| StorageError::from_io(error, "Could not reopen committed file"))?
        .sync_all()
        .await
        .map_err(|error| StorageError::from_io(error, "Could not sync committed file"))?;
    sync_directory(parent.to_path_buf()).await?;
    if let Err(error) = fs::remove_file(&transaction_path).await {
        log::warn!(
            "Committed /{}, but could not remove transaction link: {error}",
            path.trim_matches('/')
        );
    } else {
        sync_directory(state.transaction_dir.clone()).await?;
    }

    let metadata = entry_metadata_for_path(file_path).await?;
    log::info!("Atomically created /{}", path.trim_matches('/'));
    Ok((StatusCode::CREATED, Json(metadata)).into_response())
}
