use freeway::{app, config, http};
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "freeway [--config config.yaml] [--host 127.0.0.1] [--port 4100]\n\
Reads .env from the current folder (or a parent). Config path: --config, else $ROUTER_CONFIG, else ./config.yaml.";

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let (mut config_path, mut host, mut port): (Option<String>, Option<String>, Option<u16>) = (None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config_path = args.next(),
            "--host" => host = args.next(),
            "--port" => port = args.next().and_then(|p| p.parse().ok()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other:?}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let _ = dotenvy::dotenv(); // .env of the folder the server is started from (or a parent)
    let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()).to_lowercase();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(format!("warn,freeway={level}"))).init();

    let settings = match config::load_settings(config_path.as_deref()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let client = http::build_client(settings.connect_timeout);
    let (router, catalog, writer) = app::build_app_with_writer(settings.clone(), app::load_tokens(), client).await;
    app::spawn_refresh_loop(catalog, settings.refresh_minutes);

    let addr = (host.unwrap_or_else(|| settings.host.clone()), port.unwrap_or(settings.port));
    let listener = match tokio::net::TcpListener::bind((addr.0.as_str(), addr.1)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}:{}: {e}", addr.0, addr.1);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("listening on http://{}:{}", addr.0, addr.1);
    let result = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await;
    writer.flush().await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
