use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use openporch_server::{Config, VERSION};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "openporch", version = VERSION, about = "The openporch platform orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the API server.
    Serve {
        /// The configuration file. `OPENPORCH_ORG`, `OPENPORCH_DATA_DIR` and
        /// `OPENPORCH_LISTEN` override it.
        #[arg(long, env = "OPENPORCH_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Prints a new master key, for key rotation: put it first in the master key
    /// file and keep the old keys below it.
    Keygen,
    /// Prints the version.
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Keygen => {
            let key = openporch_store::secrets::MasterKey::generate();
            println!("{}", key.to_base64().as_str());
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("openporch {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Serve { config } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .json()
                .init();
            match serve(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %e, "openporch stopped");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// How long open connections may take to close once shutdown has drained.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

async fn serve(config: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config.as_deref(), |k| std::env::var(k).ok())?;
    let env_key = std::env::var("OPENPORCH_MASTER_KEY").ok();
    let listen = config.listen;
    let grace_seconds = config.shutdown_grace_seconds;
    let state = openporch_server::start(config, env_key).await?;
    let (router, specs) = openporch_server::router(state.clone());
    tracing::info!(routes = specs.len(), %listen, ready = state.ready(), "listening");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let (drained, closing) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown().await;
        // Status requests keep working while running deployments finish.
        state
            .drain(std::time::Duration::from_secs(grace_seconds))
            .await;
        let _ = drained.send(());
    });
    // Once drained, open connections get a little longer to close; a request
    // that outlives that does not hold the process up.
    let deadline = async {
        if closing.await.is_ok() {
            tokio::time::sleep(CLOSE_GRACE).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        served = server => served?,
        () = deadline => tracing::warn!("closing connections that were still open after shutdown"),
    }
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}
