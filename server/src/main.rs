#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Err(error) = server::run_from_env().await {
        log::error!("Server failed: {error}");
        std::process::exit(1);
    }
}
