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
    /// Prints the version.
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
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

async fn serve(config: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(config.as_deref(), |k| std::env::var(k).ok())?;
    let env_key = std::env::var("OPENPORCH_MASTER_KEY").ok();
    let listen = config.listen;
    let state = openporch_server::start(config, env_key).await?;
    let (router, specs) = openporch_server::router(state.clone());
    tracing::info!(routes = specs.len(), %listen, ready = state.ready(), "listening");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
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
