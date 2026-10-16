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
    /// Backs a stopped server up into a new or empty directory. The master key
    /// is not included: back it up on its own.
    Backup {
        /// The configuration file, as for `serve`.
        #[arg(long, env = "OPENPORCH_CONFIG")]
        config: Option<PathBuf>,
        /// The directory to write the backup to.
        dest: PathBuf,
    },
    /// Restores a backup into an empty data directory. The next start
    /// observes every resource before it accepts changes.
    Restore {
        /// The configuration file, as for `serve`.
        #[arg(long, env = "OPENPORCH_CONFIG")]
        config: Option<PathBuf>,
        /// The backup directory.
        backup: PathBuf,
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
            println!(
                "openporch {VERSION} (schema {})",
                openporch_store::SCHEMA_VERSION
            );
            ExitCode::SUCCESS
        }
        Command::Backup { config, dest } => {
            let done = match load(config.as_deref()) {
                Ok(config) => openporch_server::backup::backup(&config, &dest)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            match done {
                Ok(manifest) => {
                    println!(
                        "backed up {} resources of `{}` (schema {}) to {}",
                        manifest.active_resources,
                        manifest.org,
                        manifest.schema_version,
                        dest.display()
                    );
                    if !manifest.key_ids.is_empty() {
                        println!(
                            "restoring it needs the master keys {}, which are not in the backup",
                            manifest.key_ids.join(", ")
                        );
                    }
                    if manifest.state_backend != "local" {
                        println!(
                            "OpenTofu state in {} is not in the backup",
                            manifest.state_backend
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("openporch backup: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Restore { config, backup } => {
            let done = match load(config.as_deref()) {
                Ok(config) => openporch_server::backup::restore(&config, &backup)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            match done {
                Ok(manifest) => {
                    println!(
                        "restored {} resources of `{}`; the next start observes each before it \
                         accepts changes",
                        manifest.active_resources, manifest.org
                    );
                    if !manifest.key_ids.is_empty() {
                        println!("it needs the master keys {}", manifest.key_ids.join(", "));
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("openporch restore: {e}");
                    ExitCode::FAILURE
                }
            }
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

fn load(config: Option<&std::path::Path>) -> Result<Config, String> {
    Config::load(config, |k| std::env::var(k).ok()).map_err(|e| e.to_string())
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
