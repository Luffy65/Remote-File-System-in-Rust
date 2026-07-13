use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::io;

#[derive(Debug)]
pub(crate) enum StorageError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Forbidden(&'static str),
    Conflict(&'static str),
    PreconditionFailed(&'static str),
    RequestBody(&'static str),
    Io(io::Error),
}

impl StorageError {
    pub(crate) fn from_io(error: io::Error, not_found_message: &'static str) -> Self {
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
            StorageError::PreconditionFailed(message) => {
                (StatusCode::PRECONDITION_FAILED, message).into_response()
            }
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
