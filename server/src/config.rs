use std::{env, net::SocketAddr, path::PathBuf};

const DEFAULT_STORAGE_ROOT: &str = "remote-storage";
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:3000";

pub struct ServerConfig {
    pub storage_root: PathBuf,
    pub listen_addr: SocketAddr,
    pub auth_token: Option<String>,
}

impl ServerConfig {
    pub fn from_env_args() -> Result<Self, String> {
        let storage_root = env::args()
            .nth(1)
            .or_else(|| env::var("REMOTE_FS_ROOT").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STORAGE_ROOT));
        let address =
            env::var("REMOTE_FS_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_string());
        let listen_addr = address
            .parse::<SocketAddr>()
            .map_err(|error| format!("Invalid REMOTE_FS_ADDR '{address}': {error}"))?;
        let auth_token = env::var("REMOTE_FS_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());

        if !listen_addr.ip().is_loopback() && auth_token.is_none() {
            return Err(
                "REMOTE_FS_TOKEN must be set when REMOTE_FS_ADDR listens on a non-loopback address"
                    .to_string(),
            );
        }

        Ok(ServerConfig {
            storage_root,
            listen_addr,
            auth_token,
        })
    }
}
