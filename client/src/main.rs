mod api;
mod cache;
mod fuse;

use fuser::MountOption;
use log;
use std::{
    env, io,
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
fn spawn_daemon(mountpoint: &str, server_url: &str) -> io::Result<()> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--serve-daemon")
        .arg(mountpoint)
        .arg(server_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    command.spawn()?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_daemon(_mountpoint: &str, _server_url: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "daemon mode is only supported on Unix platforms",
    ))
}

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
    let args: Vec<String> = env::args().collect();
    let program_name = args.get(0).map_or("remote-fs-client", |s| s.as_str());
    let spawn_daemon_mode = args.get(1).is_some_and(|arg| arg == "--daemon");
    let serve_daemon_mode = args.get(1).is_some_and(|arg| arg == "--serve-daemon");
    let mountpoint_index = if spawn_daemon_mode || serve_daemon_mode {
        2
    } else {
        1
    };

    if args.len() <= mountpoint_index {
        eprintln!(
            "Usage: {} [--daemon] <MOUNTPOINT> [SERVER_URL]",
            program_name
        );
        std::process::exit(1);
    }

    let mountpoint = &args[mountpoint_index];
    let server_url = args.get(mountpoint_index + 1).map_or_else(
        || {
            log::info!("No server URL provided, defaulting to http://localhost:3000");
            "http://localhost:3000".to_string()
        },
        |url| url.clone(),
    );

    if spawn_daemon_mode {
        if let Err(error) = spawn_daemon(mountpoint, &server_url) {
            eprintln!("Failed to start daemon: {}", error);
            std::process::exit(1);
        }
        return;
    }

    env_logger::init();

    log::info!("Mounting to {}, server URL: {}", mountpoint, server_url);

    let options = vec![
        MountOption::FSName("remoteFS".to_string()),
        MountOption::AutoUnmount,
        //MountOption::AllowRoot, // Optional: consider security implications
    ];

    let fs = fuse::RemoteFs::new(&server_url);

    if serve_daemon_mode {
        if let Err(e) = fuser::mount2(fs, mountpoint, &options) {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let session = match fuser::spawn_mount2(fs, mountpoint, &options) {
        Ok(session) => session,
        Err(e) => {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
    };

    log::info!("Filesystem mounted successfully on {}.", mountpoint);

    let shutdown_runtime =
        tokio::runtime::Runtime::new().expect("Failed to create shutdown runtime");
    shutdown_runtime.block_on(wait_for_shutdown_signal());

    log::info!("Unmounting filesystem from {}", mountpoint);
    let _ = session.join();
}
