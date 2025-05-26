mod fuse; // Ensure fuse module is declared
mod api;  // Ensure api module is declared

use std::env;
use fuser::MountOption;
use log;

// The RemoteFs struct and its Filesystem impl are now in fuse.rs

fn main() {
    env_logger::init(); // Initialize logger

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <MOUNTPOINT> [SERVER_URL]", args.get(0).map_or("remote-fs-client", |s| s.as_str()));
        std::process::exit(1);
    }

    let mountpoint = &args[1];
    let server_url = args.get(2).map_or_else(
        || {
            log::info!("No server URL provided, defaulting to http://localhost:3000");
            "http://localhost:3000".to_string()
        },
        |url| url.clone()
    );

    log::info!("Mounting to {}, server URL: {}", mountpoint, server_url);

    let options = vec![
        MountOption::FSName("remoteFS".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowRoot, // Optional: consider security implications
    ];

    // Correct instantiation based on client/src/fuse.rs RemoteFs::new which does not return a Result
    let fs = fuse::RemoteFs::new(&server_url);

    match fuser::mount2(fs, mountpoint, &options) {
        Ok(_) => {
            log::info!("Filesystem mounted successfully on {}. Press Ctrl-C to unmount.", mountpoint);
            // mount2 blocks until unmounted, so nothing more to do here for a simple client.
            // For a more complex application, you might join a thread or handle signals.
        }
        Err(e) => {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
    }
}
