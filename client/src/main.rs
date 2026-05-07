mod api;
mod cache;
mod fuse; // Ensure fuse module is declared
// Ensure api module is declared

use fuser::MountOption;
use log;
use std::env;

// The RemoteFs struct and its Filesystem impl are now in fuse.rs

async fn wait_for_shutdown_signal() {
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

fn main() {
    env_logger::init(); // Initialize logger

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!(
            "Usage: {} <MOUNTPOINT> [SERVER_URL]",
            args.get(0).map_or("remote-fs-client", |s| s.as_str())
        );
        std::process::exit(1);
    }

    let mountpoint = &args[1];
    let server_url = args.get(2).map_or_else(
        || {
            log::info!("No server URL provided, defaulting to http://localhost:3000");
            "http://localhost:3000".to_string()
        },
        |url| url.clone(),
    );

    log::info!("Mounting to {}, server URL: {}", mountpoint, server_url);

    let options = vec![
        MountOption::FSName("remoteFS".to_string()),
        MountOption::AutoUnmount,
        //MountOption::AllowRoot, // Optional: consider security implications
    ];

    // Correct instantiation based on client/src/fuse.rs RemoteFs::new which does not return a Result
    let fs = fuse::RemoteFs::new(&server_url);

    let session = match fuser::spawn_mount2(fs, mountpoint, &options) {
        Ok(session) => session,
        Err(e) => {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
    };

    log::info!(
        "Filesystem mounted successfully on {}. Press Ctrl-C to unmount.",
        mountpoint
    );

    let shutdown_runtime =
        tokio::runtime::Runtime::new().expect("Failed to create shutdown runtime");
    shutdown_runtime.block_on(wait_for_shutdown_signal());

    log::info!("Unmounting filesystem from {}", mountpoint);
    let _ = session.join();
}
