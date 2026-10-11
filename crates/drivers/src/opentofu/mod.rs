//! `openporch/opentofu`: provisions a resource by running OpenTofu code from a Git
//! repository or an inline script.
//!
//! Each call gets a fresh owner-only run directory under
//! `<root>/runs/<guresid>`. The module's backend is overridden with orchestrator
//! state under `<root>/state/<guresid>`, which also keeps the dependency lock
//! file and, for inline scripts, the script last applied. The cookie keeps the
//! resolved source commit, so deletion plans against what was applied rather
//! than the current branch tip. Secret variables go into an owner-only variables
//! file, never onto a command line, and every log line and error is redacted
//! before it leaves the driver.

mod exec;
mod files;
mod git;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use self::exec::RunLog;
use crate::{
    Action, Capabilities, Cookie, DeleteOutcome, Driver, DriverError, DriverFuture, DriverInfo,
    DriverOutputs, DriverRequest, Observation, Plan, PutOutcome, SecretMap, Targets, any_object,
};

/// The cookie format: [`Retained`] as JSON.
const COOKIE_VERSION: u32 = 1;
/// The lines of stderr an error carries.
const ERROR_LINES: usize = 20;
/// The most log appended to an error with `append_logs_to_error`.
const MAX_ERROR_LOG_BYTES: usize = 64 * 1024;
/// Error codes.
const CODE_FAILED: &str = "TOFU-001";
const CODE_PERMIT: &str = "TOFU-002";
const CODE_INPUT: &str = "TOFU-003";
/// The override file that points the module at orchestrator state.
const BACKEND_OVERRIDE: &str = "openporch_backend_override.tf";
const LOCK_FILE: &str = ".terraform.lock.hcl";
const RETAINED_SCRIPT: &str = "script.tf";

/// Where and how the driver runs OpenTofu.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `tofu` binary.
    pub tofu: PathBuf,
    /// The `git` binary.
    pub git: PathBuf,
    /// Holds run directories, state and the provider plugin cache.
    pub root: PathBuf,
    /// Allows `file://` sources. Only for tests: it lets a definition read the
    /// server's files.
    pub allow_file_sources: bool,
}

impl Config {
    /// `tofu` and `git` from the search path, keeping everything under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            tofu: "tofu".into(),
            git: "git".into(),
            root: root.into(),
            allow_file_sources: false,
        }
    }
}

/// The OpenTofu driver.
#[derive(Debug)]
pub struct OpenTofu {
    info: DriverInfo,
    config: Config,
}

impl OpenTofu {
    pub fn new(config: Config) -> Self {
        Self {
            info: DriverInfo {
                id: "openporch/opentofu".into(),
                protocol_version: "builtin".into(),
                cookie_version: COOKIE_VERSION,
                input_schema: any_object(),
                output_schema: any_object(),
                capabilities: Capabilities {
                    targets: Targets {
                        resources: true,
                        workloads: true,
                        triggers: false,
                    },
                    observe: true,
                    // State converges a retried apply on the same objects.
                    recover: true,
                    preview: true,
                    cancel: false,
                },
            },
            config,
        }
    }

    fn state_dir(&self, req: &DriverRequest) -> PathBuf {
        self.config.root.join("state").join(req.guresid.as_str())
    }

    fn state_file(&self, req: &DriverRequest) -> PathBuf {
        self.state_dir(req).join("terraform.tfstate")
    }
}

/// A failure inside a run, before redaction.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Bad inputs; nothing ran.
    Input(String),
    /// A command failed: its name, what it was doing and its stderr.
    Command { what: String, stderr: String },
    /// The plan needs a permit the request does not carry.
    Permit(String),
    /// The local machine failed.
    Io(String),
}

impl Failure {
    fn io(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }

    fn command(program: &str, what: &str, stderr: &str) -> Self {
        Self::Command {
            what: format!("{program}: {what}"),
            stderr: stderr.to_owned(),
        }
    }
}

/// Parsed driver inputs.
struct Inputs {
    source: Option<(git::Source, Option<String>)>,
    script: Option<String>,
    variables: Map<String, Value>,
    secret_variables: Map<String, Value>,
    files: Vec<(String, String, bool)>,
    credentials_config: Option<CredentialsConfig>,
    tokens: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CredentialsConfig {
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    variables: BTreeMap<String, String>,
}

/// What a successful run retains for later calls.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
struct Retained {
    /// The source URL, path within it and the commit applied.
    source: Option<RetainedSource>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct RetainedSource {
    url: String,
    path: Option<String>,
    commit: String,
}

fn input(message: impl Into<String>) -> Failure {
    Failure::Input(message.into())
}

fn string_map(v: Option<&Value>, what: &str) -> Result<Map<String, Value>, Failure> {
    match v {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(input(format!("`{what}` must be a map"))),
    }
}

fn optional_string(
    m: &Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<Option<String>, Failure> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(input(format!("`{what}` must be a string"))),
    }
}

fn parse(config: &Config, req: &DriverRequest) -> Result<Inputs, Failure> {
    let values = &req.values;
    let secrets = req.secrets.expose();
    for key in values.keys() {
        if !matches!(
            key.as_str(),
            "source"
                | "script"
                | "variables"
                | "files"
                | "credentials_config"
                | "backend"
                | "append_logs_to_error"
                | "timeout"
        ) {
            return Err(input(format!("unknown OpenTofu driver input `{key}`")));
        }
    }
    if values.get("backend").is_some_and(|b| !b.is_null()) {
        return Err(input(
            "`backend` is not supported yet: state is always orchestrator-managed",
        ));
    }
    let source_values = string_map(values.get("source"), "source")?;
    let source_secrets = string_map(secrets.get("source"), "secrets.source")?;
    let source = match optional_string(&source_values, "url", "source.url")? {
        None if source_values.is_empty() => None,
        None => return Err(input("`source.url` is required when `source` is set")),
        Some(url) => {
            git::check_url(&url, config.allow_file_sources).map_err(Failure::Input)?;
            let rev = optional_string(&source_values, "rev", "source.rev")?
                .unwrap_or_else(|| "HEAD".into());
            if rev.starts_with('-')
                || !rev
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
            {
                return Err(input("`source.rev` must be a branch, tag or commit"));
            }
            let path = optional_string(&source_values, "path", "source.path")?;
            if let Some(p) = &path
                && files::relative(p).is_none()
            {
                return Err(input(
                    "`source.path` must be a relative path inside the repository",
                ));
            }
            Some((
                git::Source {
                    url,
                    rev,
                    username: optional_string(&source_values, "username", "source.username")?,
                    password: optional_string(
                        &source_secrets,
                        "password",
                        "secrets.source.password",
                    )?,
                    ssh_key: optional_string(&source_secrets, "ssh_key", "secrets.source.ssh_key")?,
                },
                path,
            ))
        }
    };
    let script = optional_string(values, "script", "script")?;
    if source.is_none() && script.is_none() {
        return Err(input("set `source`, `script` or both"));
    }
    let mut files = Vec::new();
    for (map, secret) in [
        (string_map(values.get("files"), "files")?, false),
        (string_map(secrets.get("files"), "secrets.files")?, true),
    ] {
        for (path, content) in map {
            let Value::String(content) = content else {
                return Err(input(format!("file `{path}` must be a string")));
            };
            if files::relative(&path).is_none() {
                return Err(input(format!(
                    "file `{path}` must be a relative path inside the module"
                )));
            }
            files.push((path, content, secret));
        }
    }
    let credentials_config = match values.get("credentials_config") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone()).map_err(|_| {
            input("`credentials_config` takes `environment`, `file` and `variables`")
        })?),
    };
    let mut tokens = BTreeMap::new();
    for (host, token) in string_map(secrets.get("tokens"), "secrets.tokens")? {
        let Value::String(token) = token else {
            return Err(input(format!("token for `{host}` must be a string")));
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        {
            return Err(input(format!("`{host}` is not a host name")));
        }
        tokens.insert(host, token);
    }
    Ok(Inputs {
        source,
        script,
        variables: string_map(values.get("variables"), "variables")?,
        secret_variables: string_map(secrets.get("variables"), "secrets.variables")?,
        files,
        credentials_config,
        tokens,
    })
}

/// The string values in `v`, at any depth.
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(m) => m.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

fn secret_strings(maps: &[&SecretMap]) -> Vec<String> {
    let mut out = Vec::new();
    for m in maps {
        m.expose().values().for_each(|v| strings(v, &mut out));
    }
    out
}

/// One prepared run: its directory, module directory, environment and
/// variable files.
struct Run {
    dir: PathBuf,
    module: PathBuf,
    env: BTreeMap<String, String>,
    var_files: Vec<String>,
    retained: Retained,
    log: RunLog,
}

impl Drop for Run {
    /// Run directories hold secret files and are not reused.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// How the run's source is chosen.
enum SourceAt<'a> {
    /// The definition's current source and revision.
    Current,
    /// What a previous apply retained.
    Retained(&'a Retained),
}

impl OpenTofu {
    /// Creates the run directory and a log that redacts the request's secrets.
    fn start(&self, req: &DriverRequest) -> Result<Run, Failure> {
        let mut log = RunLog::default();
        let mut redact = secret_strings(&[&req.secrets, &req.resource_secrets]);
        if let Some(c) = &req.credentials {
            redact.extend(secret_strings(&[c]));
        }
        log.redact_values(redact);

        let op: String = Sha256::digest(req.operation.id.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = self
            .config
            .root
            .join("runs")
            .join(req.guresid.as_str())
            .join(op);
        let _ = fs::remove_dir_all(&dir);
        files::private_dir(&dir).map_err(Failure::io)?;
        Ok(Run {
            module: dir.join("module"),
            dir,
            env: BTreeMap::new(),
            var_files: Vec::new(),
            retained: Retained::default(),
            log,
        })
    }

    /// Prepares the run directory with source, files, variables and backend.
    async fn prepare(
        &self,
        run: &mut Run,
        req: &DriverRequest,
        at: SourceAt<'_>,
    ) -> Result<(), Failure> {
        let inputs = parse(&self.config, req)?;
        let home = run.dir.join("home");
        files::private_dir(&home).map_err(Failure::io)?;
        let state = self.state_dir(req);
        files::private_dir(&state).map_err(Failure::io)?;

        // Source: fetched at the requested or retained revision, or the script.
        let script = match at {
            SourceAt::Current => inputs.script.clone(),
            SourceAt::Retained(r) if r.source.is_some() => inputs.script.clone(),
            SourceAt::Retained(_) => {
                Some(fs::read_to_string(state.join(RETAINED_SCRIPT)).map_err(|_| {
                    input("the script this resource was applied with is not retained; its state is kept")
                })?)
            }
        };
        let source = match (&inputs.source, at) {
            (_, SourceAt::Retained(Retained { source: Some(s) })) => {
                let (current, _) = inputs.source.as_ref().ok_or_else(|| {
                    input("the definition no longer has a `source`; its credentials are needed to fetch the retained commit")
                })?;
                Some((
                    git::Source {
                        url: s.url.clone(),
                        rev: s.commit.clone(),
                        username: current.username.clone(),
                        password: current.password.clone(),
                        ssh_key: current.ssh_key.clone(),
                    },
                    s.path.clone(),
                ))
            }
            (Some((s, path)), _) => Some((
                git::Source {
                    url: s.url.clone(),
                    rev: s.rev.clone(),
                    username: s.username.clone(),
                    password: s.password.clone(),
                    ssh_key: s.ssh_key.clone(),
                },
                path.clone(),
            )),
            (None, _) => None,
        };
        match &source {
            Some((s, path)) => {
                let checkout = run.dir.join("source");
                let commit =
                    git::fetch(&self.config, &mut run.log, s, &s.rev, &checkout, &home).await?;
                run.module = match path {
                    Some(p) => files::inside(&checkout, p).map_err(Failure::Input)?,
                    None => checkout.canonicalize().map_err(Failure::io)?,
                };
                if !run.module.is_dir() {
                    return Err(input("`source.path` is not a directory in the repository"));
                }
                run.retained.source = Some(RetainedSource {
                    url: s.url.clone(),
                    path: path.clone(),
                    commit,
                });
            }
            None => files::private_dir(&run.module).map_err(Failure::io)?,
        }
        if let Some(script) = &script {
            let name = if source.is_some() {
                "override.tf"
            } else {
                "main.tf"
            };
            files::write_private(&run.module.join(name), script.as_bytes()).map_err(Failure::io)?;
        }
        for (path, content, _) in &inputs.files {
            let target = files::inside(&run.module, path).map_err(Failure::Input)?;
            files::write_private(&target, content.as_bytes()).map_err(Failure::io)?;
        }

        // State and lock file live outside the run directory.
        let backend = format!(
            "terraform {{\n  backend \"local\" {{\n    path = {}\n  }}\n}}\n",
            json!(self.state_file(req).display().to_string())
        );
        files::write_private(&run.module.join(BACKEND_OVERRIDE), backend.as_bytes())
            .map_err(Failure::io)?;
        let lock = state.join(LOCK_FILE);
        if lock.exists() {
            fs::copy(&lock, run.module.join(LOCK_FILE)).map_err(Failure::io)?;
        }

        // Credentials: as configured, or the default AWS environment.
        let mut secret_variables = inputs.secret_variables.clone();
        if let Some(creds) = &req.credentials {
            let creds = creds.expose();
            let field = |name: &str| -> Result<String, Failure> {
                match creds.get(name) {
                    Some(Value::String(s)) => Ok(s.clone()),
                    _ => Err(input(format!(
                        "the cloud account has no credential field `{name}`"
                    ))),
                }
            };
            match &inputs.credentials_config {
                Some(cc) => {
                    for (env, name) in &cc.environment {
                        check_env_name(env)?;
                        run.env.insert(env.clone(), field(name)?);
                    }
                    for (var, name) in &cc.variables {
                        secret_variables.insert(var.clone(), Value::String(field(name)?));
                    }
                    if let Some(var) = &cc.file {
                        let path = run.dir.join("credentials.json");
                        files::write_private(
                            &path,
                            Value::Object(creds.clone()).to_string().as_bytes(),
                        )
                        .map_err(Failure::io)?;
                        secret_variables
                            .insert(var.clone(), Value::String(path.display().to_string()));
                    }
                }
                None if creds.contains_key("AccessKeyId") => {
                    for (env, name) in [
                        ("AWS_ACCESS_KEY_ID", "AccessKeyId"),
                        ("AWS_SECRET_ACCESS_KEY", "SecretAccessKey"),
                        ("AWS_SESSION_TOKEN", "SessionToken"),
                        ("AWS_REGION", "Region"),
                    ] {
                        if let Some(Value::String(v)) = creds.get(name)
                            && !v.is_empty()
                        {
                            run.env.insert(env.into(), v.clone());
                        }
                    }
                }
                None => {}
            }
        } else if inputs.credentials_config.is_some() {
            return Err(input("`credentials_config` needs a `driver_account`"));
        }
        for (host, token) in &inputs.tokens {
            let name = host.replace('-', "__").replace('.', "_");
            run.env.insert(format!("TF_TOKEN_{name}"), token.clone());
        }
        run.env.insert("HOME".into(), home.display().to_string());
        run.env.insert("TF_IN_AUTOMATION".into(), "1".into());
        run.env.insert("TF_INPUT".into(), "0".into());
        let cache = self.config.root.join("plugin-cache");
        files::private_dir(&cache).map_err(Failure::io)?;
        run.env
            .insert("TF_PLUGIN_CACHE_DIR".into(), cache.display().to_string());

        // Variables: plain and secret in separate files, both outside the module.
        let mut redact = Vec::new();
        secret_variables
            .values()
            .for_each(|v| strings(v, &mut redact));
        run.log.redact_values(redact);
        for (name, vars) in [
            ("values.tfvars.json", &inputs.variables),
            ("secrets.tfvars.json", &secret_variables),
        ] {
            if vars.is_empty() {
                continue;
            }
            let path = run.dir.join(name);
            files::write_private(&path, Value::Object(vars.clone()).to_string().as_bytes())
                .map_err(Failure::io)?;
            run.var_files.push(format!("-var-file={}", path.display()));
        }
        Ok(())
    }

    /// Starts a run and prepares it, then runs `body`; the log goes to the
    /// request's sink and failures become redacted driver errors.
    async fn with_run<T>(
        &self,
        req: &DriverRequest,
        at: SourceAt<'_>,
        body: impl AsyncFnOnce(&mut Run) -> Result<T, Failure>,
    ) -> Result<T, DriverError> {
        let mut run = match self.start(req) {
            Ok(run) => run,
            Err(f) => return self.finish(req, &RunLog::default(), Err(f), false),
        };
        let result = match self.prepare(&mut run, req, at).await {
            Ok(()) => body(&mut run).await,
            Err(f) => Err(f),
        };
        self.finish(req, &run.log, result, Self::append_logs(req))
    }

    async fn tofu(
        &self,
        run: &mut Run,
        what: &str,
        args: &[&str],
    ) -> Result<exec::Output, Failure> {
        let mut all: Vec<&str> = args.to_vec();
        all.push("-no-color");
        let out = exec::run(&mut run.log, &self.config.tofu, &all, &run.module, &run.env)
            .await
            .map_err(|e| Failure::Io(format!("running `tofu`: {e}")))?;
        if !out.success {
            return Err(Failure::command("tofu", what, &out.stderr));
        }
        Ok(out)
    }

    /// `init`, then a saved plan; returns its actions and fingerprint.
    async fn plan_run(
        &self,
        run: &mut Run,
        destroy: bool,
    ) -> Result<(Vec<(Action, String)>, String), Failure> {
        self.tofu(run, "init failed", &["init", "-input=false"])
            .await?;
        let mut args = vec!["plan", "-input=false", "-out=openporch.tfplan"];
        if destroy {
            args.push("-destroy");
        }
        let var_files = run.var_files.clone();
        args.extend(var_files.iter().map(String::as_str));
        self.tofu(run, "plan failed", &args).await?;
        let shown = self
            .tofu(
                run,
                "reading the plan failed",
                &["show", "-json", "openporch.tfplan"],
            )
            .await?;
        let plan: Value = serde_json::from_str(&shown.stdout)
            .map_err(|_| Failure::Io("`tofu show -json` printed no plan".into()))?;
        let mut actions = Vec::new();
        for change in plan["resource_changes"].as_array().into_iter().flatten() {
            let address = change["address"].as_str().unwrap_or_default().to_owned();
            let kinds: Vec<&str> = change["change"]["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let action = match kinds.as_slice() {
                ["create"] => Action::Create,
                ["update"] => Action::Update,
                ["delete"] => Action::Delete,
                ["delete", "create"] | ["create", "delete"] => Action::Replace,
                _ => continue,
            };
            actions.push((action, address));
        }
        actions.sort_by(|a, b| a.1.cmp(&b.1));
        Ok((actions.clone(), fingerprint(&actions)))
    }

    /// Applies the saved plan, refusing destructive changes without a matching
    /// permit.
    async fn apply_run(
        &self,
        run: &mut Run,
        req: &DriverRequest,
        actions: &[(Action, String)],
        fp: &str,
    ) -> Result<(), Failure> {
        let destructive: Vec<&str> = actions
            .iter()
            .filter(|(a, _)| matches!(a, Action::Delete | Action::Replace))
            .map(|(_, addr)| addr.as_str())
            .collect();
        if !destructive.is_empty() && req.permit.as_deref() != Some(fp) {
            return Err(Failure::Permit(format!(
                "the plan destroys or replaces {}; it needs a permit for plan {fp}",
                destructive.join(", ")
            )));
        }
        self.tofu(
            run,
            "apply failed",
            &["apply", "-input=false", "openporch.tfplan"],
        )
        .await?;
        Ok(())
    }

    /// Reads outputs after an apply; sensitive ones become secrets and are
    /// redacted from the log.
    async fn outputs(&self, run: &mut Run) -> Result<DriverOutputs, Failure> {
        let out = self
            .tofu(run, "reading outputs failed", &["output", "-json"])
            .await?;
        let outputs: Map<String, Value> = serde_json::from_str(&out.stdout)
            .map_err(|_| Failure::Io("`tofu output -json` printed no outputs".into()))?;
        Ok(split_outputs(outputs, &mut run.log))
    }

    /// Keeps what later calls need, once the apply succeeded.
    fn retain(&self, req: &DriverRequest, run: &Run) -> Result<Option<Cookie>, Failure> {
        let state = self.state_dir(req);
        let lock = run.module.join(LOCK_FILE);
        if lock.exists() {
            fs::copy(&lock, state.join(LOCK_FILE)).map_err(Failure::io)?;
        }
        if run.retained.source.is_none() {
            let script = fs::read(run.module.join("main.tf")).map_err(Failure::io)?;
            files::write_private(&state.join(RETAINED_SCRIPT), &script).map_err(Failure::io)?;
        }
        let data = serde_json::to_string(&run.retained).map_err(|e| Failure::Io(e.to_string()))?;
        Cookie::new(COOKIE_VERSION, data)
            .map(Some)
            .map_err(|e| Failure::Io(e.to_string()))
    }

    /// Sends the redacted log to the request's sink and turns a failure into
    /// a driver error.
    fn finish<T>(
        &self,
        req: &DriverRequest,
        log: &RunLog,
        result: Result<T, Failure>,
        append_logs: bool,
    ) -> Result<T, DriverError> {
        for line in log.redacted().lines() {
            req.log.line(line);
        }
        result.map_err(|f| match f {
            Failure::Input(m) => DriverError::Invalid {
                code: Some(CODE_INPUT.into()),
                message: log.redact(&m),
            },
            Failure::Permit(m) => DriverError::Invalid {
                code: Some(CODE_PERMIT.into()),
                message: m,
            },
            Failure::Io(m) => DriverError::Invalid {
                code: Some(CODE_FAILED.into()),
                message: log.redact(&m),
            },
            Failure::Command { what, stderr } => {
                let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
                let tail = lines[lines.len().saturating_sub(ERROR_LINES)..].join("\n");
                let mut message = format!("{what}:\n{}", log.redact(&tail));
                if append_logs {
                    let full = log.redacted();
                    let start = full.len().saturating_sub(MAX_ERROR_LOG_BYTES);
                    let start = (start..full.len())
                        .find(|i| full.is_char_boundary(*i))
                        .unwrap_or(full.len());
                    message.push_str("\n\nFull log:\n");
                    message.push_str(&full[start..]);
                }
                DriverError::Invalid {
                    code: Some(CODE_FAILED.into()),
                    message,
                }
            }
        })
    }

    fn retained(req: &DriverRequest) -> Result<Option<Retained>, DriverError> {
        let Some(c) = &req.cookie else {
            return Ok(None);
        };
        if c.version != COOKIE_VERSION {
            return Err(DriverError::Uncertain(format!(
                "cannot read the version {} cookie of this resource",
                c.version
            )));
        }
        serde_json::from_str(&c.data)
            .map(Some)
            .map_err(|_| DriverError::Uncertain("the resource's cookie is not readable".into()))
    }

    fn append_logs(req: &DriverRequest) -> bool {
        req.values
            .get("append_logs_to_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    async fn put_inner(
        &self,
        req: &DriverRequest,
        run: &mut Run,
    ) -> Result<(DriverOutputs, Option<Cookie>), Failure> {
        let (actions, fp) = self.plan_run(run, false).await?;
        self.apply_run(run, req, &actions, &fp).await?;
        let outputs = self.outputs(run).await?;
        let cookie = self.retain(req, run)?;
        Ok((outputs, cookie))
    }
}

fn check_env_name(name: &str) -> Result<(), Failure> {
    let ok = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
        && !matches!(name, "PATH" | "HOME")
        && !name.starts_with("TF_");
    if ok {
        Ok(())
    } else {
        Err(input(format!(
            "`{name}` cannot be set by `credentials_config`"
        )))
    }
}

/// A fingerprint of planned actions.
fn fingerprint(actions: &[(Action, String)]) -> String {
    let mut h = Sha256::new();
    for (a, addr) in actions {
        h.update(format!("{a:?} {addr}\n").as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Splits `tofu output -json` (or state outputs) into values and secrets, and
/// adds sensitive values to the redaction list.
fn split_outputs(outputs: Map<String, Value>, log: &mut RunLog) -> DriverOutputs {
    let mut values = Map::new();
    let mut secrets = Map::new();
    let mut redact = Vec::new();
    for (name, o) in outputs {
        let value = o.get("value").cloned().unwrap_or(Value::Null);
        if o.get("sensitive").and_then(Value::as_bool).unwrap_or(false) {
            strings(&value, &mut redact);
            secrets.insert(name, value);
        } else {
            values.insert(name, value);
        }
    }
    log.redact_values(redact);
    DriverOutputs {
        values,
        secrets: SecretMap::new(secrets),
        manifests: Vec::new(),
    }
}

impl Driver for OpenTofu {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn plan<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async move {
            let planned = self
                .with_run(req, SourceAt::Current, async |run| {
                    self.plan_run(run, false).await
                })
                .await?;
            Ok(to_plan(planned))
        })
    }

    fn plan_delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async move {
            if !self.state_file(req).exists() {
                return Ok(Plan::NoEffects);
            }
            let retained = Self::retained(req)?.unwrap_or_default();
            let planned = self
                .with_run(req, SourceAt::Retained(&retained), async |run| {
                    self.plan_run(run, true).await
                })
                .await?;
            Ok(to_plan(planned))
        })
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            let (outputs, cookie) = self
                .with_run(req, SourceAt::Current, async |run| {
                    self.put_inner(req, run).await
                })
                .await?;
            Ok(PutOutcome::Ready { outputs, cookie })
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            let path = self.state_file(req);
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Observation::Absent);
                }
                Err(_) => {
                    return Ok(Observation::Unknown {
                        reason: "the resource's state cannot be read".into(),
                    });
                }
            };
            let Ok(state) = serde_json::from_str::<Value>(&text) else {
                return Ok(Observation::Unknown {
                    reason: "the resource's state is not valid JSON".into(),
                });
            };
            let outputs = state["outputs"].as_object().cloned().unwrap_or_default();
            Ok(Observation::Ready {
                outputs: split_outputs(outputs, &mut RunLog::default()),
                cookie: None,
            })
        })
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async move {
            // Nothing was applied, or an earlier delete finished.
            if !self.state_file(req).exists() {
                return Ok(DeleteOutcome::Deleted);
            }
            let retained = Self::retained(req)?.unwrap_or_default();
            self.with_run(req, SourceAt::Retained(&retained), async |run| {
                let (actions, fp) = self.plan_run(run, true).await?;
                self.apply_run(run, req, &actions, &fp).await?;
                // Only confirmed destruction removes the state.
                fs::remove_dir_all(self.state_dir(req)).map_err(Failure::io)
            })
            .await?;
            Ok(DeleteOutcome::Deleted)
        })
    }
}

fn to_plan((actions, fingerprint): (Vec<(Action, String)>, String)) -> Plan {
    if actions.is_empty() {
        Plan::NoEffects
    } else {
        Plan::Actions {
            actions,
            fingerprint,
        }
    }
}

#[cfg(test)]
mod tests;
