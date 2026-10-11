//! `openporch/opentofu`: provisions a resource by running OpenTofu code from a Git
//! repository or an inline script.
//!
//! Each call gets a fresh owner-only run directory under
//! `<root>/runs/<guresid>`. The module's backend, whatever it declares, is
//! overridden with orchestrator state: `<root>/state/<guresid>` locally, or a
//! workspace named by the GUResID in an S3 bucket ([`StateBackend`]).
//! `<root>/state/<guresid>` also keeps the dependency lock file, the script and
//! the resolved source commit last applied. They are kept
//! before an apply starts, so deletion plans against what was applied rather
//! than the current branch tip even after a partial apply; the cookie carries
//! the same record. Secret variables go into an owner-only variables
//! file, never onto a command line, and every log line and error is redacted
//! before it leaves the driver.

mod backend;
mod exec;
mod files;
mod git;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use self::backend::BackendId;
pub use self::backend::{S3Credentials, S3State, StateBackend};
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
const CODE_BACKEND: &str = "TOFU-004";
/// The override file that points the module at orchestrator state.
const BACKEND_OVERRIDE: &str = "openporch_backend_override.tf";
const LOCK_FILE: &str = ".terraform.lock.hcl";
const RETAINED_SCRIPT: &str = "script.tf";
/// What the last apply ran, as [`Retained`] JSON.
const RETAINED_FILE: &str = "retained.json";
/// Marks state removed by a confirmed delete, so a repeated delete is a no-op
/// while state lost otherwise is not mistaken for "never existed".
const DELETED_MARKER: &str = "deleted";

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
    /// Host keys SSH sources are checked against. Without it SSH sources fail.
    pub ssh_known_hosts: Option<PathBuf>,
    /// Where state lives. Recovery metadata (the retained script, lock file
    /// and source commit) stays under `root` either way.
    pub state: StateBackend,
}

impl Config {
    /// `tofu` and `git` from the search path, keeping everything under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            tofu: "tofu".into(),
            git: "git".into(),
            root: root.into(),
            allow_file_sources: false,
            ssh_known_hosts: None,
            state: StateBackend::Local,
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
                input_schema: json!({
                    "type": "object",
                    "anyOf": [{"required": ["source"]}, {"required": ["script"]}],
                    "properties": {
                        "source": {"type": "object", "required": ["url"]},
                        "script": {"type": "string"},
                        "variables": {"type": "object"},
                        "files": {"type": "object", "additionalProperties": {"type": "string"}},
                        "credentials_config": {"type": "object"},
                        "append_logs_to_error": {"type": "boolean"},
                    }
                }),
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

    /// The resource's state, and if there is none, whether that is expected.
    /// Refuses state kept in a different backend than the one configured.
    async fn state(&self, req: &DriverRequest) -> Result<State, DriverError> {
        let configured = self.config.state.id();
        if let Some(kept) = self.retained(req)?.backend
            && kept != configured
        {
            return Err(DriverError::Invalid {
                code: Some(CODE_BACKEND.into()),
                message: format!(
                    "this resource's state is in {kept}, but the driver is configured for \
                     {configured}; move the state before deploying it"
                ),
            });
        }
        if let Some(state) = self.read_state(req).await? {
            return Ok(State::Present(state));
        }
        let unreadable = |_| DriverError::Uncertain("the resource's state cannot be read".into());
        let deleted = self
            .state_dir(req)
            .join(DELETED_MARKER)
            .try_exists()
            .map_err(unreadable)?;
        // A cookie means an apply succeeded, so its state must exist.
        if req.cookie.is_some() && !deleted {
            return Err(DriverError::Uncertain(
                "the resource was applied but its OpenTofu state is missing; it needs recovery"
                    .into(),
            ));
        }
        Ok(State::Absent)
    }
}

/// See [`OpenTofu::state`].
#[derive(PartialEq)]
enum State {
    /// The state document.
    Present(Value),
    /// Never applied, or deleted.
    Absent,
}

/// A failure inside a run, before redaction.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Bad inputs; nothing ran.
    Input(String),
    /// A command failed: its name, what it was doing and its stderr.
    Command { what: String, stderr: String },
    /// A failure once an apply started: infrastructure may have changed.
    Uncertain { what: String, stderr: String },
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

    /// This failure, happening after an apply started.
    fn after_apply(self) -> Self {
        match self {
            Self::Command { what, stderr } | Self::Uncertain { what, stderr } => {
                Self::Uncertain { what, stderr }
            }
            Self::Input(what) | Self::Permit(what) | Self::Io(what) => Self::Uncertain {
                what,
                stderr: String::new(),
            },
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
    /// Where the state is. Absent in records from before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<BackendId>,
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
            let Some(rel) = files::relative(&path) else {
                return Err(input(format!(
                    "file `{path}` must be a relative path inside the module"
                )));
            };
            let name = rel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let tofu_owned = rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with(".terraform"));
            if rel.components().count() == 1 && (name == "main.tf" || is_override(&name))
                || tofu_owned
            {
                return Err(input(format!(
                    "file `{path}` would replace the script, the backend or OpenTofu's own files"
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

/// Whether OpenTofu loads `name` as an override file, which can replace the
/// orchestrator's backend.
fn is_override(name: &str) -> bool {
    let stem = name
        .strip_suffix(".tf")
        .or_else(|| name.strip_suffix(".tf.json"));
    stem.is_some_and(|s| s == "override" || s.ends_with("_override"))
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
    /// `-backend-config` arguments for `init`.
    init_args: Vec<String>,
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
#[derive(Clone, Copy)]
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

        // Unique per call, so concurrent calls never share a directory.
        let dir = self
            .config
            .root
            .join("runs")
            .join(req.guresid.as_str())
            .join(format!("{:016x}", rand::random::<u64>()));
        files::private_dir(&dir).map_err(Failure::io)?;
        Ok(Run {
            module: dir.join("module"),
            dir,
            env: BTreeMap::new(),
            var_files: Vec::new(),
            init_args: Vec::new(),
            retained: Retained {
                source: None,
                backend: Some(self.config.state.id()),
            },
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
            SourceAt::Retained(r) => match fs::read_to_string(state.join(RETAINED_SCRIPT)) {
                Ok(script) => Some(script),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && r.source.is_some() => None,
                Err(_) => {
                    return Err(input(
                        "the script this resource was applied with is not retained; its state is kept",
                    ));
                }
            },
        };
        let source = match (&inputs.source, at) {
            (
                _,
                SourceAt::Retained(Retained {
                    source: Some(s), ..
                }),
            ) => {
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
                for entry in fs::read_dir(&run.module).map_err(Failure::io)? {
                    let name = entry.map_err(Failure::io)?.file_name();
                    let name = name.to_string_lossy();
                    if is_override(&name) {
                        return Err(input(format!(
                            "the module has an override file `{name}`; it could replace the orchestrator's backend"
                        )));
                    }
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

        self.prepare_backend(run, req, &home)?;
        // The lock file applied last, unless the module brings its own.
        let lock = state.join(LOCK_FILE);
        let module_lock = run.module.join(LOCK_FILE);
        let use_retained = match at {
            SourceAt::Retained(_) => true,
            SourceAt::Current => !module_lock.exists(),
        };
        if use_retained && lock.exists() {
            fs::copy(&lock, &module_lock).map_err(Failure::io)?;
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

    /// Points the module at orchestrator state, which lives outside the run
    /// directory, and sets the environment every run shares.
    fn prepare_backend(
        &self,
        run: &mut Run,
        req: &DriverRequest,
        home: &std::path::Path,
    ) -> Result<(), Failure> {
        let backend = &self.config.state;
        backend.check().map_err(Failure::Io)?;
        let text = backend.override_file(&self.state_file(req));
        files::write_private(&run.module.join(BACKEND_OVERRIDE), text.as_bytes())
            .map_err(Failure::io)?;
        if let Some((creds, redact)) = backend.credentials_file().map_err(Failure::Io)? {
            run.log.redact_values(redact);
            let path = run.dir.join("backend.tfbackend");
            files::write_private(&path, creds.as_bytes()).map_err(Failure::io)?;
            run.init_args
                .push(format!("-backend-config={}", path.display()));
        }
        run.env.insert("HOME".into(), home.display().to_string());
        run.env.insert("TF_IN_AUTOMATION".into(), "1".into());
        run.env.insert("TF_INPUT".into(), "0".into());
        let cache = self.config.root.join("plugin-cache");
        files::private_dir(&cache).map_err(Failure::io)?;
        run.env
            .insert("TF_PLUGIN_CACHE_DIR".into(), cache.display().to_string());
        Ok(())
    }

    /// `tofu init` with the backend configuration.
    async fn init(&self, run: &mut Run) -> Result<(), Failure> {
        let mut args = vec!["init".to_owned(), "-input=false".to_owned()];
        args.extend(run.init_args.iter().cloned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tofu(run, "init failed", &args, exec::Stdout::Logged)
            .await?;
        Ok(())
    }

    /// Whether the resource's workspace exists; only with S3 state.
    async fn has_workspace(&self, run: &mut Run, req: &DriverRequest) -> Result<bool, Failure> {
        let list = self
            .tofu(
                run,
                "listing workspaces failed",
                &["workspace", "list"],
                exec::Stdout::Logged,
            )
            .await?;
        Ok(list
            .stdout
            .lines()
            .any(|l| l.trim_start_matches('*').trim() == req.guresid.as_str()))
    }

    /// The resource's state document, if it has one.
    async fn read_state(&self, req: &DriverRequest) -> Result<Option<Value>, DriverError> {
        let unreadable = || DriverError::Uncertain("the resource's state is not valid JSON".into());
        if !self.config.state.workspaces() {
            return match fs::read_to_string(self.state_file(req)) {
                Ok(text) => serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|_| unreadable()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(DriverError::Uncertain(
                    "the resource's state cannot be read".into(),
                )),
            };
        }
        // Remote state: a module holding only the backend, so no source or
        // provider is needed. Workspaces are listed first, because selecting
        // a missing one creates it.
        let mut run = match self.start(req) {
            Ok(run) => run,
            Err(f) => return self.finish(req, &RunLog::default(), Err(f), false),
        };
        let result = async {
            let home = run.dir.join("home");
            files::private_dir(&home).map_err(Failure::io)?;
            files::private_dir(&run.module).map_err(Failure::io)?;
            self.prepare_backend(&mut run, req, &home)?;
            self.init(&mut run).await?;
            if !self.has_workspace(&mut run, req).await? {
                return Ok(None);
            }
            run.env
                .insert("TF_WORKSPACE".into(), req.guresid.as_str().to_owned());
            let out = self
                .tofu(
                    &mut run,
                    "reading state failed",
                    &["state", "pull"],
                    exec::Stdout::Withheld,
                )
                .await?;
            if out.stdout.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&out.stdout)
                .map(Some)
                .map_err(|_| Failure::Io("`tofu state pull` printed no state".into()))
        }
        .await;
        self.finish(req, &run.log, result, false)
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
        let result = result.and_then(|t| self.protect_local_state(req).map(|()| t));
        self.finish(req, &run.log, result, Self::append_logs(req))
    }

    /// Makes local state files owner-only; OpenTofu writes them world-readable
    /// (inside an owner-only directory).
    fn protect_local_state(&self, req: &DriverRequest) -> Result<(), Failure> {
        use std::os::unix::fs::PermissionsExt as _;
        if self.config.state.workspaces() {
            return Ok(());
        }
        let file = self.state_file(req);
        let backup = file.with_extension("tfstate.backup");
        for path in [file, backup] {
            match fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(Failure::io(e).after_apply());
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn tofu(
        &self,
        run: &mut Run,
        what: &str,
        args: &[&str],
        stdout: exec::Stdout,
    ) -> Result<exec::Output, Failure> {
        let mut all: Vec<&str> = args.to_vec();
        all.push("-no-color");
        let out = exec::run(
            &mut run.log,
            &self.config.tofu,
            &all,
            &run.module,
            &run.env,
            stdout,
        )
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
        req: &DriverRequest,
        destroy: bool,
    ) -> Result<(Vec<(Action, String)>, String), Failure> {
        self.init(run).await?;
        if self.config.state.workspaces() {
            self.tofu(
                run,
                "selecting the resource's workspace failed",
                &[
                    "workspace",
                    "select",
                    "-or-create=true",
                    req.guresid.as_str(),
                ],
                exec::Stdout::Logged,
            )
            .await?;
            run.env
                .insert("TF_WORKSPACE".into(), req.guresid.as_str().to_owned());
        }
        let state = self
            .tofu(
                run,
                "reading state failed",
                &["state", "pull"],
                exec::Stdout::Withheld,
            )
            .await?;
        let state_version = serde_json::from_str::<Value>(&state.stdout).map_or_else(
            |_| "none".into(),
            |s| format!("{} {}", s["lineage"], s["serial"]),
        );
        let mut args = vec!["plan", "-input=false", "-out=openporch.tfplan"];
        if destroy {
            args.push("-destroy");
        }
        let var_files = run.var_files.clone();
        args.extend(var_files.iter().map(String::as_str));
        self.tofu(run, "plan failed", &args, exec::Stdout::Logged)
            .await?;
        let shown = self
            .tofu(
                run,
                "reading the plan failed",
                &["show", "-json", "openporch.tfplan"],
                exec::Stdout::Withheld,
            )
            .await?;
        let plan: Value = serde_json::from_str(&shown.stdout)
            .map_err(|_| Failure::Io("`tofu show -json` printed no plan".into()))?;
        let mut actions = Vec::new();
        // What the permit covers: the resource, the call, the state it plans
        // against, the inputs and each change with its reasons. Values are left
        // out, so the fingerprint reveals nothing about secrets.
        let mut covered = vec![
            req.guresid.as_str().to_owned(),
            if destroy { "destroy" } else { "apply" }.to_owned(),
            req.operation.fingerprint.clone(),
            state_version,
        ];
        for change in plan["resource_changes"].as_array().into_iter().flatten() {
            let address = change["address"].as_str().unwrap_or_default().to_owned();
            let action = match kinds(&change["change"]).as_slice() {
                [] | ["no-op"] | ["read"] => continue,
                ["create"] => Action::Create,
                ["update"] => Action::Update,
                // `forget` drops an object from state, leaving it unmanaged.
                ["delete"] | ["forget"] => Action::Delete,
                // Replacements and any action this driver does not know.
                _ => Action::Replace,
            };
            covered.push(
                json!([
                    address,
                    change["change"]["actions"],
                    change["change"]["replace_paths"],
                    change["action_reason"],
                ])
                .to_string(),
            );
            actions.push((action, address));
        }
        // Outputs are effects too: a plan that only changes them still applies.
        for (name, change) in plan["output_changes"].as_object().into_iter().flatten() {
            if !matches!(kinds(change).as_slice(), [] | ["no-op"]) {
                actions.push((Action::Update, format!("output.{name}")));
            }
        }
        actions.sort_by(|a, b| a.1.cmp(&b.1));
        covered[4..].sort();
        Ok((actions, fingerprint(&covered)))
    }

    /// Applies the saved plan, refusing destructive changes without a matching
    /// permit. What the apply runs is retained first; any failure once the
    /// apply starts is uncertain.
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
        self.retain(req, run)?;
        self.tofu(
            run,
            "apply failed",
            &["apply", "-input=false", "openporch.tfplan"],
            exec::Stdout::Logged,
        )
        .await
        .map_err(Failure::after_apply)?;
        Ok(())
    }

    /// Reads outputs after an apply; sensitive ones become secrets and are
    /// redacted from the log.
    async fn outputs(&self, run: &mut Run) -> Result<DriverOutputs, Failure> {
        let out = self
            .tofu(
                run,
                "reading outputs failed",
                &["output", "-json"],
                exec::Stdout::Withheld,
            )
            .await?;
        let outputs: Map<String, Value> = serde_json::from_str(&out.stdout)
            .map_err(|_| Failure::Io("`tofu output -json` printed no outputs".into()))?;
        Ok(split_outputs(outputs, &mut run.log))
    }

    /// Keeps what later calls need: the lock file, the script and the source
    /// commit about to be applied.
    fn retain(&self, req: &DriverRequest, run: &Run) -> Result<(), Failure> {
        let state = self.state_dir(req);
        let lock = run.module.join(LOCK_FILE);
        if lock.exists() {
            let content = fs::read(&lock).map_err(Failure::io)?;
            files::write_private(&state.join(LOCK_FILE), &content).map_err(Failure::io)?;
        }
        let script_name = if run.retained.source.is_some() {
            "override.tf"
        } else {
            "main.tf"
        };
        match fs::read(run.module.join(script_name)) {
            Ok(script) => {
                files::write_private(&state.join(RETAINED_SCRIPT), &script).map_err(Failure::io)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match fs::remove_file(state.join(RETAINED_SCRIPT)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(Failure::io(e));
                    }
                    _ => {}
                }
            }
            Err(e) => return Err(Failure::io(e)),
        }
        let data = serde_json::to_string(&run.retained).map_err(|e| Failure::Io(e.to_string()))?;
        files::write_private(&state.join(RETAINED_FILE), data.as_bytes()).map_err(Failure::io)?;
        match fs::remove_file(state.join(DELETED_MARKER)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Failure::io(e)),
            _ => Ok(()),
        }
    }

    /// The cookie recording what `run` applied.
    fn cookie(run: &Run) -> Result<Option<Cookie>, Failure> {
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
        result.map_err(|f| {
            let uncertain = matches!(f, Failure::Uncertain { .. });
            match f {
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
                Failure::Command { what, stderr } | Failure::Uncertain { what, stderr } => {
                    // Redacted before cutting, so no secret is split.
                    let stderr = log.redact(&stderr);
                    let lines: Vec<&str> =
                        stderr.lines().filter(|l| !l.trim().is_empty()).collect();
                    let tail = lines[lines.len().saturating_sub(ERROR_LINES)..].join("\n");
                    let mut message = format!("{}:\n{tail}", log.redact(&what));
                    if uncertain {
                        message.insert_str(0, "infrastructure may have changed: ");
                    }
                    if append_logs {
                        let full = log.redacted();
                        let start = full.len().saturating_sub(MAX_ERROR_LOG_BYTES);
                        let start = (start..full.len())
                            .find(|i| full.is_char_boundary(*i))
                            .unwrap_or(full.len());
                        message.push_str("\n\nFull log:\n");
                        message.push_str(&full[start..]);
                    }
                    if uncertain {
                        DriverError::Uncertain(message)
                    } else {
                        DriverError::Invalid {
                            code: Some(CODE_FAILED.into()),
                            message,
                        }
                    }
                }
            }
        })
    }

    /// What the last apply ran: kept with the state, or from the cookie.
    fn retained(&self, req: &DriverRequest) -> Result<Retained, DriverError> {
        match fs::read_to_string(self.state_dir(req).join(RETAINED_FILE)) {
            Ok(text) => {
                return serde_json::from_str(&text).map_err(|_| {
                    DriverError::Uncertain("the resource's retained source is not readable".into())
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(DriverError::Uncertain(
                    "the resource's retained source cannot be read".into(),
                ));
            }
        }
        let Some(c) = &req.cookie else {
            return Ok(Retained::default());
        };
        if c.version != COOKIE_VERSION {
            return Err(DriverError::Uncertain(format!(
                "cannot read the version {} cookie of this resource",
                c.version
            )));
        }
        serde_json::from_str(&c.data)
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
        let (actions, fp) = self.plan_run(run, req, false).await?;
        self.apply_run(run, req, &actions, &fp).await?;
        let outputs = self.outputs(run).await.map_err(Failure::after_apply)?;
        let cookie = Self::cookie(run).map_err(Failure::after_apply)?;
        Ok((outputs, cookie))
    }
}

fn check_env_name(name: &str) -> Result<(), Failure> {
    let ok = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
        && !matches!(name, "PATH" | "HOME" | "AWS_EC2_METADATA_DISABLED")
        && !name.starts_with("TF_");
    if ok {
        Ok(())
    } else {
        Err(input(format!(
            "`{name}` cannot be set by `credentials_config`"
        )))
    }
}

/// A plan's fingerprint: a hash of what a permit for it covers.
fn fingerprint(covered: &[String]) -> String {
    let mut h = Sha256::new();
    for line in covered {
        h.update(line.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The action kinds of a plan change, such as `["delete", "create"]`.
fn kinds(change: &Value) -> Vec<&str> {
    change["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Every scalar in `v` as text, at any depth.
fn scalars(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Number(n) => out.push(n.to_string()),
        Value::Bool(_) | Value::Null => {}
        Value::Array(a) => a.iter().for_each(|v| scalars(v, out)),
        Value::Object(m) => m.values().for_each(|v| scalars(v, out)),
    }
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
            scalars(&value, &mut redact);
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
            self.state(req).await?;
            let planned = self
                .with_run(req, SourceAt::Current, async |run| {
                    self.plan_run(run, req, false).await
                })
                .await?;
            Ok(to_plan(planned))
        })
    }

    fn plan_delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async move {
            if self.state(req).await? == State::Absent {
                return Ok(Plan::NoEffects);
            }
            let retained = self.retained(req)?;
            let planned = self
                .with_run(req, SourceAt::Retained(&retained), async |run| {
                    self.plan_run(run, req, true).await
                })
                .await?;
            Ok(to_plan(planned))
        })
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            self.state(req).await?;
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
            let state = match self.state(req).await {
                Ok(State::Present(state)) => state,
                Ok(State::Absent) => return Ok(Observation::Absent),
                Err(e) => {
                    return Ok(Observation::Unknown {
                        reason: e.to_string(),
                    });
                }
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
            if self.state(req).await? == State::Absent {
                return Ok(DeleteOutcome::Deleted);
            }
            let retained = self.retained(req)?;
            self.with_run(req, SourceAt::Retained(&retained), async |run| {
                let (actions, fp) = self.plan_run(run, req, true).await?;
                self.apply_run(run, req, &actions, &fp).await?;
                // Only confirmed destruction removes the state, leaving a marker
                // that it was deleted. A remote workspace is deleted only while
                // its state is empty.
                if self.config.state.workspaces() {
                    run.env.remove("TF_WORKSPACE");
                    for args in [
                        ["workspace", "select", "default"].as_slice(),
                        &["workspace", "delete", req.guresid.as_str()],
                    ] {
                        self.tofu(
                            run,
                            "removing the resource's state failed",
                            args,
                            exec::Stdout::Logged,
                        )
                        .await
                        .map_err(Failure::after_apply)?;
                    }
                }
                let state = self.state_dir(req);
                let removed = fs::remove_dir_all(&state)
                    .and_then(|()| files::private_dir(&state))
                    .and_then(|()| files::write_private(&state.join(DELETED_MARKER), b""));
                removed.map_err(|e| Failure::io(e).after_apply())
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
