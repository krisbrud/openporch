//! Deploying: `porch workload deploy` (and its `score deploy` alias), `porch
//! deploy delta|deploy`, `porch logs deploy`, `porch diff sets`, `porch resources
//! graph` and `porch api`.
//!
//! Every deployment request carries the environment generation it was based on
//! and an idempotency key. A request whose response is lost is sent again with
//! the same key and content, so the server returns the deployment it already
//! accepted instead of starting another.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use openporch_client::{Client, Error as ApiError, Method, segment, with_query};
use rand::Rng as _;
use rand::distributions::Alphanumeric;
use serde_json::{Map, Value, json};

use crate::objects::read_text;
use crate::output::{self, Format, timestamp};
use crate::{Io, Session, api};

/// The `apiVersion` of Score files.
const SCORE_API_VERSION: &str = "score.dev/v1b1";

/// `porch workload`.
#[derive(Debug, Subcommand)]
pub enum WorkloadCmd {
    /// Converts a workload document or Score file into a delta for the
    /// environment (`--app`, `--env`) and deploys it.
    Deploy(WorkloadDeployArgs),
}

/// `porch score`.
#[derive(Debug, Subcommand)]
pub enum ScoreCmd {
    /// Imports a Score file and deploys it as `porch workload deploy` would.
    Deploy(WorkloadDeployArgs),
}

/// `porch workload deploy`.
#[derive(Debug, Args)]
pub struct WorkloadDeployArgs {
    /// The workload document or Score file, or `-` for standard input.
    #[arg(short, long, value_name = "FILE")]
    pub file: PathBuf,
    /// A `WorkloadOverlay` document for a Score file.
    #[arg(long, value_name = "FILE")]
    pub overlay: Option<PathBuf>,
    /// The image of a container, or of the function (`main`).
    #[arg(long = "image", value_name = "NAME=REF", value_parser = pair)]
    pub images: Vec<(String, String)>,
    /// The artifact of the function (`main`).
    #[arg(long = "artifact", value_name = "NAME=URI", value_parser = pair)]
    pub artifacts: Vec<(String, String)>,
    /// A value at a dotted path of the document, read as YAML except where it
    /// replaces a string.
    #[arg(long = "property", value_name = "PATH=VALUE", value_parser = pair)]
    pub properties: Vec<(String, String)>,
    #[command(flatten)]
    pub run: RunArgs,
}

/// How a deployment runs.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// A comment recorded with the deployment.
    #[arg(long, default_value = "")]
    pub comment: String,
    /// Follow the deployment until it finishes, and fail if it does.
    #[arg(long)]
    pub wait: bool,
}

/// `porch deploy`.
#[derive(Debug, Subcommand)]
pub enum DeployCmd {
    /// Deploys a stored delta to an environment of the application.
    Delta {
        /// The delta ID.
        id: String,
        /// The environment.
        #[arg(value_name = "ENV")]
        target: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Deploys the set of an earlier deployment of the environment (`--env`)
    /// again: by ID, `+n` for the nth attempt before the latest (`+0` is the
    /// latest), or `last-successful`.
    Deploy {
        /// The deployment.
        reference: String,
        /// Run with the configuration the deployment recorded, or with the
        /// definitions and values as they are now.
        #[arg(long, value_enum, default_value_t = Replay::Snapshot)]
        replay: Replay,
        #[command(flatten)]
        run: RunArgs,
    },
}

/// The configuration a redeployment runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Replay {
    /// What the earlier deployment recorded.
    Snapshot,
    /// The definitions and values as they are now.
    Current,
}

/// `porch logs`.
#[derive(Debug, Subcommand)]
pub enum LogsCmd {
    /// Prints a deployment's log of the environment (`--env`), following it
    /// until the deployment finishes.
    Deploy {
        /// The deployment: an ID, `+n` or `last-successful`.
        reference: String,
    },
}

/// `porch diff`.
#[derive(Debug, Subcommand)]
pub enum DiffCmd {
    /// Prints the delta that turns set A into set B. Besides set IDs, takes
    /// `deploy/.` (the latest attempt in `--env`), `deploy/+n` (n attempts
    /// before it, failures included), `deploy/last-successful` and `deploy/<id>`.
    Sets {
        /// The set to start from.
        a: String,
        /// The set to end at.
        b: String,
        /// The output format.
        #[arg(short, long, value_enum, default_value_t = Format::Yaml)]
        output: Format,
    },
}

/// `porch resources`.
#[derive(Debug, Subcommand)]
pub enum ResourcesCmd {
    /// Prints the resource graph of the environment (`--app`, `--env`).
    Graph {
        /// The output format.
        #[arg(short, long, value_enum, default_value_t = Format::Yaml)]
        output: Format,
    },
}

/// `porch api`.
#[derive(Debug, Args)]
pub struct ApiArgs {
    /// The method.
    #[arg(value_enum)]
    pub method: HttpMethod,
    /// The path, such as `/orgs/acme/apps`.
    pub path: String,
    /// A JSON body, `@FILE` to read it from a file, or `@-` for standard input.
    #[arg(short = 'd', long = "data", value_name = "BODY")]
    pub data: Option<String>,
}

/// An HTTP method for `porch api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HttpMethod {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// PATCH.
    Patch,
    /// DELETE.
    Delete,
}

fn pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err("expected NAME=VALUE".into()),
    }
}

fn pairs(list: Vec<(String, String)>) -> Map<String, Value> {
    list.into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect()
}

fn write(io: &mut Io<'_>, text: std::fmt::Arguments<'_>) -> Result<(), String> {
    io.out.write_fmt(text).map_err(|e| e.to_string())
}

/// `/orgs/{org}/apps/{app}` followed by `suffix`.
fn app_path(session: &Session, suffix: &str) -> Result<String, String> {
    Ok(format!(
        "/orgs/{}/apps/{}{suffix}",
        segment(session.org()?),
        segment(session.app()?)
    ))
}

/// `.../envs/{env}` followed by `suffix`.
fn env_path(session: &Session, env: &str, suffix: &str) -> Result<String, String> {
    app_path(session, &format!("/envs/{}{suffix}", segment(env)))
}

pub(crate) async fn workload(
    session: &Session,
    cmd: WorkloadCmd,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let WorkloadCmd::Deploy(args) = cmd;
    workload_deploy(session, args, false, io).await
}

pub(crate) async fn score(session: &Session, cmd: ScoreCmd, io: &mut Io<'_>) -> Result<(), String> {
    let ScoreCmd::Deploy(args) = cmd;
    workload_deploy(session, args, true, io).await
}

async fn workload_deploy(
    session: &Session,
    args: WorkloadDeployArgs,
    score_only: bool,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let client = session.client()?;
    let env = session.env()?.to_owned();
    let workload = read_text(&args.file, io)?;
    if score_only {
        let version = serde_yaml_ng::from_str::<Value>(&workload)
            .ok()
            .and_then(|v| {
                v.get("apiVersion")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        if version.as_deref() != Some(SCORE_API_VERSION) {
            return Err(format!(
                "{}: not a Score file (`apiVersion: {SCORE_API_VERSION}`); use `porch workload deploy`",
                args.file.display()
            ));
        }
    }
    let overlay = args
        .overlay
        .as_deref()
        .map(|f| read_text(f, io))
        .transpose()?;
    let mut request = json!({
        "env": env,
        "workload": workload,
        "overrides": {
            "images": pairs(args.images),
            "artifacts": pairs(args.artifacts),
            "properties": pairs(args.properties),
        },
    });
    if let Some(overlay) = overlay {
        request["overlay"] = Value::String(overlay);
    }
    let delta = client
        .request(Method::POST, &app_path(session, "/deltas")?, Some(&request))
        .await
        .map_err(api)?;
    let (Some(id), Some(generation)) = (delta["id"].as_str(), delta["base_generation"].as_i64())
    else {
        return Err("unexpected response from the server: no delta ID or generation".into());
    };
    let body = json!({
        "delta_id": id,
        "expected_generation": generation,
        "comment": args.run.comment,
    });
    start(session, &client, &env, body, args.run.wait, io).await
}

pub(crate) async fn deploy(
    session: &Session,
    cmd: DeployCmd,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let client = session.client()?;
    match cmd {
        DeployCmd::Delta {
            id,
            target: env,
            run,
        } => {
            let generation = generation(session, &client, &env).await?;
            let body = json!({
                "delta_id": id,
                "expected_generation": generation,
                "comment": run.comment,
            });
            start(session, &client, &env, body, run.wait, io).await
        }
        DeployCmd::Deploy {
            reference,
            replay,
            run,
        } => {
            let env = session.env()?.to_owned();
            let generation = generation(session, &client, &env).await?;
            let body = json!({
                "deploy_id": reference,
                "replay": match replay {
                    Replay::Snapshot => "snapshot",
                    Replay::Current => "current",
                },
                "expected_generation": generation,
                "comment": run.comment,
            });
            start(session, &client, &env, body, run.wait, io).await
        }
    }
}

/// The environment's current generation.
async fn generation(session: &Session, client: &Client, env: &str) -> Result<i64, String> {
    let record = client
        .request(Method::GET, &env_path(session, env, "")?, None)
        .await
        .map_err(api)?;
    record["generation"]
        .as_i64()
        .ok_or_else(|| "unexpected response from the server: no environment generation".into())
}

/// How often a request whose response was lost is sent again.
const RETRIES: u32 = 3;

/// Requests a deployment, retrying with the same key and body when the
/// response is lost, prints its ID and, with `wait`, follows it.
async fn start(
    session: &Session,
    client: &Client,
    env: &str,
    body: Value,
    wait: bool,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let path = env_path(session, env, "/deploys")?;
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let headers = [("idempotency-key", key.as_str())];
    let mut attempt = 0;
    let deployment = loop {
        match client
            .send(Method::POST, &path, Some(&body), &headers)
            .await
        {
            Ok((_, d)) => break d,
            Err(ApiError::Transport(e)) if attempt < RETRIES => {
                attempt += 1;
                let _ = writeln!(io.err, "warning: {e}; sending the same request again");
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
            Err(e) => return Err(api(e)),
        }
    };
    let id = deployment["id"]
        .as_str()
        .ok_or("unexpected response from the server: no deployment ID")?
        .to_owned();
    write(io, format_args!("Deployment {id} started.\n"))?;
    if wait {
        follow(session, client, env, &id, io).await?;
        finished(session, client, env, &id, io).await
    } else {
        Ok(())
    }
}

/// Lines asked for per page of a log.
const LOG_PAGE: u32 = 500;
/// The pause before asking again when a log has no new lines.
const LOG_POLL: Duration = Duration::from_millis(500);

/// Prints deployment `reference`'s log lines until it finishes.
async fn follow(
    session: &Session,
    client: &Client,
    env: &str,
    reference: &str,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let path = env_path(
        session,
        env,
        &format!("/deploys/{}/logs", segment(reference)),
    )?;
    let mut after = 0;
    loop {
        let page = client
            .request(
                Method::GET,
                &with_query(
                    &path,
                    &[
                        ("after", Some(after.to_string())),
                        ("limit", Some(LOG_PAGE.to_string())),
                    ],
                ),
                None,
            )
            .await
            .map_err(api)?;
        let lines = page["lines"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for line in lines {
            write(io, format_args!("{}\n", log_line(line)))?;
        }
        if page["finished"].as_bool() == Some(true) {
            return Ok(());
        }
        let next = page["next"].as_i64().unwrap_or(after);
        if lines.is_empty() || next == after {
            tokio::time::sleep(LOG_POLL).await;
        }
        after = next;
    }
}

/// One log line: time, level, the resource it is about, and the text.
fn log_line(line: &Value) -> String {
    let at = line["at"].as_i64().map(timestamp).unwrap_or_default();
    let level = line["level"].as_str().unwrap_or("info");
    let message = line["message"].as_str().unwrap_or_default();
    match line["descriptor"].as_str() {
        Some(d) => format!("{at} {level:<5} {d}: {message}"),
        None => format!("{at} {level:<5} {message}"),
    }
}

/// Reports how a deployment ended; a failure is an error.
async fn finished(
    session: &Session,
    client: &Client,
    env: &str,
    id: &str,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let d = client
        .request(
            Method::GET,
            &env_path(session, env, &format!("/deploys/{}", segment(id)))?,
            None,
        )
        .await
        .map_err(api)?;
    match d["status"].as_str() {
        Some("succeeded") => write(io, format_args!("Deployment {id} succeeded.\n")),
        Some("failed") => Err(format!(
            "deployment {id} failed: {}",
            d["error"].as_str().unwrap_or("no reason recorded")
        )),
        other => Err(format!(
            "deployment {id} ended as {}",
            other.unwrap_or("unknown")
        )),
    }
}

pub(crate) async fn logs(session: &Session, cmd: LogsCmd, io: &mut Io<'_>) -> Result<(), String> {
    let LogsCmd::Deploy { reference } = cmd;
    let client = session.client()?;
    let env = session.env()?.to_owned();
    follow(session, &client, &env, &reference, io).await
}

/// The set ID a `diff sets` argument names.
async fn set_ref(session: &Session, client: &Client, reference: &str) -> Result<String, String> {
    let Some(deploy) = reference.strip_prefix("deploy/") else {
        return Ok(reference.to_owned());
    };
    let deploy = if deploy == "." { "+0" } else { deploy };
    let env = session.env()?;
    let d = client
        .request(
            Method::GET,
            &env_path(session, env, &format!("/deploys/{}", segment(deploy)))?,
            None,
        )
        .await
        .map_err(api)?;
    d["set_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{reference} has no deployment set"))
}

pub(crate) async fn diff(session: &Session, cmd: DiffCmd, io: &mut Io<'_>) -> Result<(), String> {
    let DiffCmd::Sets { a, b, output } = cmd;
    let client = session.client()?;
    let a = set_ref(session, &client, &a).await?;
    let b = set_ref(session, &client, &b).await?;
    let delta = client
        .request(
            Method::GET,
            &app_path(
                session,
                &format!("/sets/{}/diff/{}", segment(&a), segment(&b)),
            )?,
            None,
        )
        .await
        .map_err(api)?;
    output::print(io.out, structured(output), &delta, &[])
}

/// Tables do not fit nested documents; they print as YAML.
fn structured(format: Format) -> Format {
    match format {
        Format::Table => Format::Yaml,
        other => other,
    }
}

pub(crate) async fn resources(
    session: &Session,
    cmd: ResourcesCmd,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let ResourcesCmd::Graph { output } = cmd;
    let client = session.client()?;
    let env = session.env()?.to_owned();
    let graph = client
        .request(
            Method::GET,
            &env_path(session, &env, "/resources/graph")?,
            None,
        )
        .await
        .map_err(api)?;
    output::print(io.out, structured(output), &graph, &[])
}

pub(crate) async fn raw(session: &Session, args: ApiArgs, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let body = match args.data.as_deref() {
        None => None,
        Some(data) => {
            let text = match data.strip_prefix('@') {
                Some(file) => read_text(std::path::Path::new(file), io)?,
                None => data.to_owned(),
            };
            Some(serde_json::from_str::<Value>(&text).map_err(|e| {
                format!(
                    "the body is not JSON: line {}, column {}",
                    e.line(),
                    e.column()
                )
            })?)
        }
    };
    let method = match args.method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
    };
    if !args.path.starts_with('/') {
        return Err("the path starts with `/`, such as `/orgs/acme/apps`".into());
    }
    let value = client
        .request(method, &args.path, body.as_ref())
        .await
        .map_err(api)?;
    if value.is_null() {
        return Ok(());
    }
    output::print(io.out, Format::Json, &value, &[])
}
