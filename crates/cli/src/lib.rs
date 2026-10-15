//! The `porch` command line.
//!
//! [`run`] takes its environment, standard input and output as arguments, so tests
//! run commands in-process against a dev server.

pub mod config;
pub mod deploys;
pub mod members;
pub mod objects;
pub mod output;
pub mod plans;

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use openporch_client::{Client, DevicePoll, Error as ApiError, Token, server_url, url_key};

use crate::config::{Credential, Env, Flags, Paths, Settings, TokenSource, resolve};

#[cfg(test)]
mod tests;

/// The `porch` command line.
#[derive(Debug, Parser)]
#[command(name = "porch", version, about = "The openporch command line")]
pub struct Cli {
    /// The server URL. Defaults to `OPENPORCH_URL`, then `url` in the config file.
    #[arg(long, global = true, value_name = "URL")]
    pub url: Option<String>,
    /// The organization. Defaults to `OPENPORCH_ORG`, then `org` in the config file.
    #[arg(long, global = true, value_name = "ORG")]
    pub org: Option<String>,
    /// The application. Defaults to `OPENPORCH_APP`, then `app` in the config file.
    #[arg(long, global = true, value_name = "APP")]
    pub app: Option<String>,
    /// The environment. Defaults to `OPENPORCH_ENV`, then `env` in the config file.
    #[arg(long, global = true, value_name = "ENV")]
    pub env: Option<String>,
    /// The config file. Defaults to `OPENPORCH_CONFIG`, then
    /// `~/.config/openporch/config.toml`; the credentials file sits next to it.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// An API token, before the command. Prefer `OPENPORCH_TOKEN` or
    /// `porch login --token`, which keep it out of the shell history.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
    /// The command.
    #[command(subcommand)]
    pub command: Command,
}

/// A `porch` command.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Logs in with GitHub through the server, or stores an API token with
    /// `--token`. The server URL and organization are saved to the config file
    /// when it has none.
    Login(LoginArgs),
    /// Ends the session and forgets the stored token for the server.
    Logout,
    /// Lists objects, or shows one.
    Get(objects::GetArgs),
    /// Creates an object.
    #[command(subcommand)]
    Create(objects::Create),
    /// Deletes an object. Sets and deployment records are kept for good.
    #[command(subcommand)]
    Delete(objects::Delete),
    /// Manages the organization's members and their tokens.
    #[command(subcommand)]
    Users(members::Users),
    /// Manages application users.
    #[command(subcommand)]
    Apps(members::Apps),
    /// Manages environment type deployers.
    #[command(subcommand)]
    EnvTypes(members::EnvTypes),
    /// Manages secret stores.
    #[command(subcommand)]
    Secretstores(members::SecretStores),
    /// Queries the audit log, newest first.
    AuditLogs(members::AuditLogs),
    /// Deploys workload documents.
    #[command(subcommand)]
    Workload(deploys::WorkloadCmd),
    /// Deploys Score files.
    #[command(subcommand)]
    Score(deploys::ScoreCmd),
    /// Deploys a delta, or an earlier deployment again.
    #[command(subcommand)]
    Deploy(deploys::DeployCmd),
    /// Prints logs.
    #[command(subcommand)]
    Logs(deploys::LogsCmd),
    /// Compares deployment sets.
    #[command(subcommand)]
    Diff(deploys::DiffCmd),
    /// Shows an environment's resources.
    #[command(subcommand)]
    Resources(deploys::ResourcesCmd),
    /// Sends an authenticated request to the API and prints the JSON response.
    Api(deploys::ApiArgs),
}

/// `porch login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Store an API token, read from standard input or a hidden prompt.
    #[arg(long)]
    pub token: bool,
    /// The invitation code, on a first GitHub login.
    #[arg(long, value_name = "CODE", conflicts_with = "token")]
    pub invitation: Option<String>,
}

/// The process environment of a command.
pub struct Io<'a> {
    /// Environment variables.
    pub env: Env<'a>,
    /// Standard input.
    pub stdin: &'a mut dyn BufRead,
    /// Whether standard input is a terminal, so a token is prompted for.
    pub stdin_is_terminal: bool,
    /// Standard output.
    pub out: &'a mut dyn Write,
    /// Standard error.
    pub err: &'a mut dyn Write,
}

/// Runs `porch` with `args` (including the program name) and returns its exit code.
pub async fn run<I, T>(args: I, io: &mut Io<'_>) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let text = scrubbed(&e);
            let target: &mut dyn Write = if e.use_stderr() { io.err } else { io.out };
            let _ = target.write_all(text.as_bytes());
            return e.exit_code();
        }
    };
    match execute(cli, io).await {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(io.err, "error: {message}");
            1
        }
    }
}

/// A parse error's text without the arguments it quotes, which may be a token:
/// `porch login --token <value>` is an easy mistake.
fn scrubbed(e: &clap::Error) -> String {
    use clap::error::{ContextKind, ContextValue};
    let mut text = e.render().to_string();
    for kind in [ContextKind::InvalidArg, ContextKind::InvalidValue] {
        let Some(ContextValue::String(arg)) = e.get(kind) else {
            continue;
        };
        // A flag name is safe to show; a value, or `--flag=value`'s value, is not.
        let value = match arg.strip_prefix('-') {
            Some(_) => arg.split_once('=').map(|(_, v)| v).unwrap_or_default(),
            None => arg.as_str(),
        };
        if !value.is_empty() {
            text = text.replace(value, "<hidden>");
        }
    }
    text
}

/// What a command needs: the files and the resolved settings.
struct Session {
    paths: Paths,
    settings: Settings,
}

impl Session {
    fn url(&self) -> Result<&str, String> {
        self.settings.url.as_deref().ok_or_else(|| {
            "no server URL: pass --url, set OPENPORCH_URL or set `url` in the config file"
                .to_owned()
        })
    }

    fn org(&self) -> Result<&str, String> {
        self.settings.org.as_deref().ok_or_else(|| {
            "no organization: pass --org, set OPENPORCH_ORG or set `org` in the config file"
                .to_owned()
        })
    }

    fn app(&self) -> Result<&str, String> {
        self.settings.app.as_deref().ok_or_else(|| {
            "no application: pass --app, set OPENPORCH_APP or set `app` in the config file"
                .to_owned()
        })
    }

    fn env(&self) -> Result<&str, String> {
        self.settings.env.as_deref().ok_or_else(|| {
            "no environment: pass --env, set OPENPORCH_ENV or set `env` in the config file"
                .to_owned()
        })
    }

    /// A client sending the resolved token.
    fn client(&self) -> Result<Client, String> {
        let (url, org) = (self.url()?, self.org()?);
        let token = self
            .settings
            .token
            .as_ref()
            .map(|(t, _)| t.clone())
            .ok_or_else(|| "not logged in: run `porch login`, or set OPENPORCH_TOKEN".to_owned())?;
        Client::new(url, org, Some(token)).map_err(|e| e.to_string())
    }
}

/// The message for a failed API call, with a hint for rejected credentials.
pub(crate) fn api(e: ApiError) -> String {
    match e.status() {
        Some(401) => format!("{e}; run `porch login` or check OPENPORCH_TOKEN"),
        _ => e.to_string(),
    }
}

async fn execute(cli: Cli, io: &mut Io<'_>) -> Result<(), String> {
    if cli.token.is_some() {
        let _ = writeln!(
            io.err,
            "warning: --token can end up in your shell history; prefer OPENPORCH_TOKEN or `porch login --token`"
        );
    }
    let paths = Paths::locate(cli.config.as_deref(), io.env)?;
    let file = paths.read_config()?;
    let creds = paths.read_credentials()?;
    let flags = Flags {
        url: cli.url,
        org: cli.org,
        app: cli.app,
        env: cli.env,
        token: cli.token.and_then(Token::new),
    };
    let session = Session {
        settings: resolve(&flags, io.env, &file, &creds),
        paths,
    };
    match cli.command {
        Command::Login(args) if args.token => login_token(&session, io).await,
        Command::Login(args) => login_device(&session, args.invitation.as_deref(), io).await,
        Command::Logout => logout(&session, io).await,
        Command::Get(args) => objects::get(&session, args, io).await,
        Command::Create(what) => objects::create(&session, what, io).await,
        Command::Delete(what) => objects::delete(&session, what, io).await,
        Command::Users(cmd) => members::users(&session, cmd, io).await,
        Command::Apps(cmd) => members::apps(&session, cmd, io).await,
        Command::EnvTypes(cmd) => members::env_types(&session, cmd, io).await,
        Command::Secretstores(cmd) => members::secret_stores(&session, cmd, io).await,
        Command::AuditLogs(args) => members::audit_logs(&session, args, io).await,
        Command::Workload(cmd) => deploys::workload(&session, cmd, io).await,
        Command::Score(cmd) => deploys::score(&session, cmd, io).await,
        Command::Deploy(cmd) => deploys::deploy(&session, cmd, io).await,
        Command::Logs(cmd) => deploys::logs(&session, cmd, io).await,
        Command::Diff(cmd) => deploys::diff(&session, cmd, io).await,
        Command::Resources(cmd) => deploys::resources(&session, cmd, io).await,
        Command::Api(args) => deploys::raw(&session, args, io).await,
    }
}

/// Reads a token from standard input, or from a hidden prompt on a terminal.
fn read_token(io: &mut Io<'_>) -> Result<Token, String> {
    let value = if io.stdin_is_terminal {
        rpassword::prompt_password("Token: ").map_err(|e| format!("reading the token: {e}"))?
    } else {
        let mut line = String::new();
        io.stdin
            .read_line(&mut line)
            .map_err(|e| format!("reading the token from standard input: {e}"))?;
        line
    };
    Token::new(value).ok_or_else(|| "no token given".to_owned())
}

/// Stores `credential` for the server and fills in the config file's URL and
/// organization when it has none.
fn remember(session: &Session, credential: Credential) -> Result<(), String> {
    let url = server_url(session.url()?).map_err(|e| e.to_string())?;
    let mut creds = session.paths.read_credentials()?;
    creds.servers.insert(url_key(&url), credential);
    session.paths.write_credentials(&creds)?;
    let mut file = session.paths.read_config()?;
    if file.url.is_none() && file.org.is_none() {
        file.url = Some(url_key(&url));
        file.org = session.settings.org.clone();
        session.paths.write_config(&file)?;
    }
    Ok(())
}

async fn login_token(session: &Session, io: &mut Io<'_>) -> Result<(), String> {
    let (url, org) = (session.url()?, session.org()?);
    let token = read_token(io)?;
    let client = Client::new(url, org, Some(token.clone())).map_err(|e| e.to_string())?;
    // Only a token the server accepts is stored.
    let user = client.current_user().await.map_err(|e| match e.status() {
        Some(401) => "the server rejected the token".to_owned(),
        _ => e.to_string(),
    })?;
    let id = user["id"].as_str().unwrap_or_default().to_owned();
    remember(
        session,
        Credential {
            token: token.expose().to_owned(),
            kind: "api".into(),
            user: Some(id.clone()),
            expires_at: None,
        },
    )?;
    writeln!(io.out, "Logged in to {} as {id}.", url_key(client.url())).map_err(|e| e.to_string())
}

/// The longest a device login waits for the person.
const MAX_LOGIN_SECS: u64 = 3600;
/// The longest wait between two polls.
const MAX_POLL_SECS: u64 = 60;

async fn login_device(
    session: &Session,
    invitation: Option<&str>,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let (url, org) = (session.url()?, session.org()?);
    let client = Client::new(url, org, None).map_err(|e| e.to_string())?;
    let device = client.start_device_login().await.map_err(api)?;
    writeln!(
        io.err,
        "Open {} and enter the code {}",
        device.verification_uri, device.user_code
    )
    .map_err(|e| e.to_string())?;
    // The server's timings are bounded, so a bad reply cannot stall the CLI.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(device.expires_in.min(MAX_LOGIN_SECS));
    let initial = device.interval.clamp(1, MAX_POLL_SECS);
    let mut interval = initial;
    let login = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(Duration::from_secs(interval).min(left)).await;
        if Instant::now() >= deadline {
            return Err("the login code expired; run `porch login` again".into());
        }
        match client
            .poll_device_login(&device.device_code, invitation)
            .await
            .map_err(|e| e.to_string())?
        {
            DevicePoll::Pending { interval: next } => {
                interval = next.clamp(initial, MAX_POLL_SECS);
            }
            DevicePoll::Session(s) => break s,
        }
    };
    remember(
        session,
        Credential {
            token: login.token.expose().to_owned(),
            kind: "session".into(),
            user: Some(login.user.clone()),
            expires_at: Some(login.expires_at),
        },
    )?;
    writeln!(
        io.out,
        "Logged in to {} as {}.",
        url_key(client.url()),
        login.user
    )
    .map_err(|e| e.to_string())
}

async fn logout(session: &Session, io: &mut Io<'_>) -> Result<(), String> {
    let url = server_url(session.url()?).map_err(|e| e.to_string())?;
    let key = url_key(&url);
    let mut creds = session.paths.read_credentials()?;
    let Some(stored) = creds.servers.remove(&key) else {
        return writeln!(io.out, "Not logged in to {key}.").map_err(|e| e.to_string());
    };
    if stored.kind == "session"
        && let (Ok(org), Some(token)) = (session.org(), Token::new(stored.token.clone()))
    {
        let client = Client::new(&key, org, Some(token)).map_err(|e| e.to_string())?;
        // An expired or revoked session is already over.
        if let Err(e) = client.end_session().await
            && e.status() != Some(401)
        {
            let _ = writeln!(io.err, "warning: the server did not end the session: {e}");
        }
    }
    session.paths.write_credentials(&creds)?;
    if session
        .settings
        .token
        .as_ref()
        .is_some_and(|(_, source)| *source != TokenSource::File)
    {
        let _ = writeln!(
            io.err,
            "note: a token is still set by --token or OPENPORCH_TOKEN"
        );
    }
    writeln!(io.out, "Logged out of {key}.").map_err(|e| e.to_string())
}
