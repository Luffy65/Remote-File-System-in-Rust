mod api;
#[cfg(not(windows))]
mod cache;
#[cfg(not(windows))]
mod fuse;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use fuser::MountOption;
#[cfg(not(windows))]
use std::{
    io,
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

struct ClientArgs {
    mountpoint: String,
    server_url: String,
    spawn_daemon_mode: bool,
    #[cfg(not(windows))]
    serve_daemon_mode: bool,
}

fn parse_args() -> ClientArgs {
    let args: Vec<String> = std::env::args().collect();
    let program_name = args.first().map_or("remote-fs-client", |s| s.as_str());
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

    ClientArgs {
        mountpoint: args[mountpoint_index].clone(),
        server_url: args
            .get(mountpoint_index + 1)
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string()),
        spawn_daemon_mode,
        #[cfg(not(windows))]
        serve_daemon_mode,
    }
}

#[cfg(unix)]
fn spawn_daemon(mountpoint: &str, server_url: &str) -> io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
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

#[cfg(all(not(unix), not(windows)))]
fn spawn_daemon(_mountpoint: &str, _server_url: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "daemon mode is only supported on Unix platforms",
    ))
}

#[cfg(not(windows))]
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

#[cfg(windows)]
fn main() {
    use std::process::Command;

    let args = parse_args();
    if args.spawn_daemon_mode {
        let result =
            Command::new(std::env::current_exe().expect("failed to locate current executable"))
                .arg("--serve-daemon")
                .arg(&args.mountpoint)
                .arg(&args.server_url)
                .spawn();
        if let Err(error) = result {
            eprintln!("Failed to start daemon: {error}");
            std::process::exit(1);
        }
        return;
    }

    env_logger::init();

    if let Err(error) = windows::run(&args.mountpoint, &args.server_url) {
        log::error!("Failed to mount Windows filesystem: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    let args = parse_args();
    if args.spawn_daemon_mode {
        if let Err(error) = spawn_daemon(&args.mountpoint, &args.server_url) {
            eprintln!("Failed to start daemon: {}", error);
            std::process::exit(1);
        }
        return;
    }

    env_logger::init();

    log::info!(
        "Mounting to {}, server URL: {}",
        args.mountpoint,
        args.server_url
    );

    let options = vec![
        MountOption::FSName("remoteFS".to_string()),
        #[cfg(not(target_os = "linux"))]
        MountOption::AutoUnmount,
    ];

    // Create the FUSE filesystem instance
    let fs = fuse::RemoteFs::new(&args.server_url);

    if args.serve_daemon_mode {
        if let Err(e) = fuser::mount2(fs, &args.mountpoint, &options) {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let session = match fuser::spawn_mount2(fs, &args.mountpoint, &options) {
        Ok(session) => session,
        Err(e) => {
            log::error!("Failed to mount filesystem: {}", e);
            std::process::exit(1);
        }
    };

    log::info!("Filesystem mounted successfully on {}.", args.mountpoint);

    // Wait for shutdown signal in a separate runtime to avoid blocking the FUSE session
    let shutdown_signal_runtime =
        tokio::runtime::Runtime::new().expect("Failed to create shutdown runtime");
    shutdown_signal_runtime.block_on(wait_for_shutdown_signal());

    log::info!("Unmounting filesystem from {}", args.mountpoint);
    session.join();
}
