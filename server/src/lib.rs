mod auth;
mod config;
mod error;
mod handlers;
mod metadata;
mod path_security;
mod transaction;

use auth::require_authentication;
pub use config::ServerConfig;
#[cfg(test)]
pub(crate) use error::StorageError;
use handlers::{
    delete_directory, delete_path, get_file, get_metadata, list_path, list_root, make_directory,
    rename_entry, update_metadata, write_file,
};

use axum::{
    body::Body,
    http::{HeaderValue, Request},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Router,
};
use remote_fs_protocol::{PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER};
#[cfg(test)]
use std::{env, time::UNIX_EPOCH};
use std::{io, path::PathBuf, sync::Arc};

pub(crate) const INTERNAL_DIR_NAME: &str = ".remote-fs-transactions";

pub(crate) struct AppState {
    pub(crate) root_dir: PathBuf,
    pub(crate) transaction_dir: PathBuf,
    pub(crate) auth_token: Option<String>,
    pub(crate) mutation_lock: tokio::sync::Mutex<()>,
}

impl AppState {
    pub(crate) fn with_auth(root_dir: PathBuf, auth_token: Option<String>) -> Self {
        let transaction_dir = root_dir.join(INTERNAL_DIR_NAME);
        std::fs::create_dir_all(&transaction_dir)
            .expect("failed to create server transaction directory");
        AppState {
            root_dir,
            transaction_dir,
            auth_token,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }
}

async fn add_protocol_version(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        PROTOCOL_VERSION_HEADER,
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    response
}

pub(crate) fn build_app(shared_state: Arc<AppState>) -> Router {
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
        .layer(middleware::from_fn(add_protocol_version))
        .layer(middleware::from_fn_with_state(
            authentication_state,
            require_authentication,
        ))
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

pub async fn run(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&config.storage_root)?;
    let storage_root = std::fs::canonicalize(&config.storage_root)?;
    let app = build_app(Arc::new(AppState::with_auth(
        storage_root.clone(),
        config.auth_token.clone(),
    )));

    log::info!("Server storage root: {}", storage_root.display());
    log::info!("Server listening on {}", config.listen_addr);
    log::info!("Protocol version: {PROTOCOL_VERSION}");
    log::info!(
        "Bearer-token authentication: {}",
        if config.auth_token.is_some() {
            "enabled"
        } else {
            "disabled (loopback only)"
        }
    );

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

pub async fn run_from_env() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig::from_env_args().map_err(io::Error::other)?;
    run(config).await
}

#[cfg(test)]
mod tests;
