//! `porch get`, `porch create` and `porch delete`: the organization's objects.
//!
//! Create and delete verbs exist only where the API lifecycle has them. Sets and
//! deployment records are immutable, so they can be read but not deleted.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use openporch_client::{Method, segment};
use serde_json::{Value, json};

use crate::output::{self, Format};
use crate::{Io, Session, api};

/// The `apiVersion` of openporch documents.
pub const API_VERSION: &str = "openporch.dev/v1";

/// What `porch get` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// Applications.
    #[value(alias = "app")]
    Apps,
    /// Environments of an application.
    #[value(alias = "env")]
    Envs,
    /// Environment types.
    #[value(alias = "env-type")]
    EnvTypes,
    /// Deployments of an environment.
    #[value(alias = "deploy")]
    Deploys,
    /// Deltas of an application.
    #[value(alias = "delta")]
    Deltas,
    /// Deployment sets, by ID.
    #[value(alias = "set")]
    Sets,
    /// Active resources of an environment.
    #[value(alias = "active-resource")]
    ActiveResources,
    /// Resource definitions.
    #[value(alias = "resource-definition", alias = "defs")]
    ResourceDefinitions,
    /// Resource types.
    #[value(alias = "resource-type")]
    ResourceTypes,
    /// Resource classes.
    #[value(alias = "resource-class")]
    ResourceClasses,
    /// Cloud accounts.
    #[value(alias = "cloud-account")]
    CloudAccounts,
}

/// `porch get`.
#[derive(Debug, Args)]
pub struct GetArgs {
    /// What to list.
    #[arg(value_enum)]
    pub kind: Kind,
    /// One object's ID instead of the list. Active resources are `TYPE/RES_ID`.
    pub id: Option<String>,
    /// The output format. Sets default to YAML, everything else to a table.
    #[arg(short, long, value_enum)]
    pub output: Option<Format>,
}

/// `porch create`.
#[derive(Debug, Subcommand)]
pub enum Create {
    /// An application. You become its owner.
    App {
        /// The application ID.
        id: String,
        /// The display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// An environment of the application (`--app`).
    Env {
        /// The environment ID.
        id: String,
        /// The environment type.
        #[arg(long = "type", value_name = "TYPE")]
        env_type: String,
        /// Start from the deployment set of this deployment of the application.
        #[arg(long, value_name = "DEPLOY_ID")]
        from_deploy: Option<String>,
    },
    /// An environment type.
    EnvType {
        /// The environment type ID.
        id: String,
        /// A description.
        #[arg(long)]
        description: Option<String>,
    },
    /// A resource definition from a `kind: Definition` YAML document.
    ResourceDefinition(FileArg),
    /// A resource type from a `kind: ResourceType` YAML document.
    ResourceType(FileArg),
    /// A cloud account from a `kind: CloudAccount` YAML document.
    CloudAccount(FileArg),
}

/// A document file.
#[derive(Debug, Args)]
pub struct FileArg {
    /// The YAML or JSON file, or `-` for standard input.
    #[arg(short, long, value_name = "FILE")]
    pub file: PathBuf,
}

/// `porch delete`.
#[derive(Debug, Subcommand)]
pub enum Delete {
    /// An application, deleting every environment and its resources.
    App {
        /// The application ID.
        id: String,
    },
    /// An environment of the application (`--app`) and its resources.
    Env {
        /// The environment ID.
        id: String,
    },
    /// An unused environment type.
    EnvType {
        /// The environment type ID.
        id: String,
    },
    /// An unused resource definition.
    ResourceDefinition {
        /// The definition ID.
        id: String,
    },
    /// An unused custom resource type.
    ResourceType {
        /// The type ID.
        id: String,
    },
    /// An unused cloud account.
    CloudAccount {
        /// The account ID.
        id: String,
    },
}

/// Columns for a table of each kind.
fn columns(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Apps => &[("ID", "id"), ("NAME", "name"), ("STATE", "state")],
        Kind::Envs => &[
            ("ID", "id"),
            ("TYPE", "type"),
            ("CONVERGENCE", "convergence"),
            ("LAST SUCCESSFUL DEPLOY", "last_successful_deploy_id"),
            ("STATE", "state"),
        ],
        Kind::EnvTypes => &[("ID", "id"), ("DESCRIPTION", "description")],
        Kind::Deploys => &[
            ("ID", "id"),
            ("KIND", "kind"),
            ("STATUS", "status"),
            ("ACTOR", "actor"),
            ("SET", "set_id"),
            ("CREATED", "created_at"),
        ],
        Kind::Deltas => &[
            ("ID", "id"),
            ("ENV", "env"),
            ("FROZEN", "frozen"),
            ("CREATED BY", "created_by"),
        ],
        Kind::Sets => &[("ID", "id")],
        Kind::ActiveResources => &[
            ("DESCRIPTOR", "descriptor"),
            ("DEFINITION", "definition_id"),
            ("STATUS", "status"),
            ("OWNERSHIP", "ownership"),
        ],
        Kind::ResourceDefinitions => &[
            ("ID", "id"),
            ("TYPE", "type"),
            ("DRIVER", "driver_type"),
            ("ACCOUNT", "driver_account"),
            ("VERSION", "version"),
        ],
        Kind::ResourceTypes => &[("ID", "id"), ("NAME", "name"), ("CATEGORY", "category")],
        Kind::ResourceClasses => &[
            ("TYPE", "resource_type"),
            ("ID", "id"),
            ("DESCRIPTION", "description"),
        ],
        Kind::CloudAccounts => &[("ID", "id"), ("NAME", "name"), ("TYPE", "type")],
    }
}

/// The path that lists `kind`, or reads object `id` of it.
fn path(session: &Session, kind: Kind, id: Option<&str>) -> Result<String, String> {
    let org = |suffix: &str| -> Result<String, String> {
        Ok(format!("/orgs/{}{suffix}", segment(session.org()?)))
    };
    let one = |base: String| match id {
        Some(id) => format!("{base}/{}", segment(id)),
        None => base,
    };
    Ok(match kind {
        Kind::Apps => one(org("/apps")?),
        Kind::Envs => one(org(&format!("/apps/{}/envs", segment(session.app()?)))?),
        Kind::EnvTypes => one(org("/env-types")?),
        Kind::Deploys => one(org(&format!(
            "/apps/{}/envs/{}/deploys",
            segment(session.app()?),
            segment(session.env()?)
        ))?),
        Kind::Deltas => one(org(&format!("/apps/{}/deltas", segment(session.app()?)))?),
        Kind::Sets => {
            let id = id.ok_or(
                "sets are read by ID: `porch get sets <ID>`; `porch get deploys` shows the set of each deployment",
            )?;
            org(&format!(
                "/apps/{}/sets/{}",
                segment(session.app()?),
                segment(id)
            ))?
        }
        Kind::ActiveResources => {
            let base = org(&format!(
                "/apps/{}/envs/{}/resources",
                segment(session.app()?),
                segment(session.env()?)
            ))?;
            match id {
                None => base,
                Some(id) => {
                    let (ty, res) = id
                        .rsplit_once('/')
                        .ok_or("an active resource is named `TYPE/RES_ID`")?;
                    format!("{base}/{}/{}", segment(ty), segment(res))
                }
            }
        }
        Kind::ResourceDefinitions => one(org("/resources/defs")?),
        Kind::ResourceTypes => one(org("/resources/types")?),
        Kind::ResourceClasses => {
            if id.is_some() {
                return Err("resource classes are listed, not read one by one".into());
            }
            org("/resources/classes")?
        }
        Kind::CloudAccounts => one(org("/resources/accounts")?),
    })
}

/// `porch get`.
pub(crate) async fn get(session: &Session, args: GetArgs, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let path = path(session, args.kind, args.id.as_deref())?;
    let value = client
        .request(Method::GET, &path, None)
        .await
        .map_err(api)?;
    let format = args.output.unwrap_or(if args.kind == Kind::Sets {
        Format::Yaml
    } else {
        Format::Table
    });
    output::print(io.out, format, &value, columns(args.kind))?;
    if format == Format::Table
        && let Some(next) = value.get("next").filter(|n| !n.is_null())
    {
        writeln!(
            io.err,
            "More results: use `porch api get '{path}?before={next}'`"
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Reads a YAML or JSON document from `file` (`-` for standard input).
pub fn read_document(file: &Path, io: &mut Io<'_>) -> Result<Value, String> {
    let text = if file == Path::new("-") {
        let mut text = String::new();
        std::io::Read::read_to_string(io.stdin, &mut text)
            .map_err(|e| format!("reading standard input: {e}"))?;
        text
    } else {
        std::fs::read_to_string(file).map_err(|e| format!("reading {}: {e}", file.display()))?
    };
    // A document can hold secret inputs, so parser errors give only a location.
    serde_yaml_ng::from_str(&text).map_err(|e| match e.location() {
        Some(l) => format!(
            "{}: not a valid YAML document (line {}, column {})",
            file.display(),
            l.line(),
            l.column()
        ),
        None => format!("{}: not a valid YAML document", file.display()),
    })
}

/// The API body of a `kind: <kind>` document:
///
/// ```yaml
/// apiVersion: openporch.dev/v1
/// kind: Definition
/// metadata:
///   id: stg-redis
/// entity:
///   type: redis
///   driver_type: openporch/opentofu
///   criteria: [{env_type: staging}]
/// ```
///
/// `metadata.id` becomes `id`, `metadata.name` the default `name`, and `entity`
/// the rest of the body.
pub fn document_body(doc: &Value, kind: &str) -> Result<Value, String> {
    match doc.get("apiVersion").and_then(Value::as_str) {
        None | Some(API_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "unsupported apiVersion `{other}`; use `{API_VERSION}`"
            ));
        }
    }
    match doc.get("kind").and_then(Value::as_str) {
        Some(k) if k == kind => {}
        Some(other) => return Err(format!("expected `kind: {kind}`, found `kind: {other}`")),
        None => return Err(format!("the document needs `kind: {kind}`")),
    }
    for key in doc.as_object().map(|o| o.keys()).into_iter().flatten() {
        if !matches!(key.as_str(), "apiVersion" | "kind" | "metadata" | "entity") {
            return Err(format!(
                "unknown top-level field `{key}`; expected apiVersion, kind, metadata and entity"
            ));
        }
    }
    let id = doc
        .pointer("/metadata/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("the document needs `metadata.id`")?;
    let mut body = match doc.get("entity") {
        Some(Value::Object(entity)) => entity.clone(),
        Some(_) => return Err("`entity` must be a map".into()),
        None => return Err("the document needs an `entity` map".into()),
    };
    if body.contains_key("id") {
        return Err("set the ID in `metadata.id`, not in `entity`".into());
    }
    body.insert("id".into(), json!(id));
    if let Some(name) = doc.pointer("/metadata/name").and_then(Value::as_str) {
        body.entry("name").or_insert_with(|| json!(name));
    }
    Ok(Value::Object(body))
}

/// `porch create`.
pub(crate) async fn create(session: &Session, what: Create, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let org = format!("/orgs/{}", segment(session.org()?));
    let (path, body, noun) = match what {
        Create::App { id, name } => {
            let mut body = json!({"id": id});
            if let Some(name) = name {
                body["name"] = json!(name);
            }
            (format!("{org}/apps"), body, "application")
        }
        Create::Env {
            id,
            env_type,
            from_deploy,
        } => {
            let mut body = json!({"id": id, "type": env_type});
            if let Some(deploy) = from_deploy {
                body["from_deploy_id"] = json!(deploy);
            }
            (
                format!("{org}/apps/{}/envs", segment(session.app()?)),
                body,
                "environment",
            )
        }
        Create::EnvType { id, description } => {
            let mut body = json!({"id": id});
            if let Some(d) = description {
                body["description"] = json!(d);
            }
            (format!("{org}/env-types"), body, "environment type")
        }
        Create::ResourceDefinition(f) => (
            format!("{org}/resources/defs"),
            document_body(&read_document(&f.file, io)?, "Definition")?,
            "resource definition",
        ),
        Create::ResourceType(f) => (
            format!("{org}/resources/types"),
            document_body(&read_document(&f.file, io)?, "ResourceType")?,
            "resource type",
        ),
        Create::CloudAccount(f) => (
            format!("{org}/resources/accounts"),
            document_body(&read_document(&f.file, io)?, "CloudAccount")?,
            "cloud account",
        ),
    };
    let created = client
        .request(Method::POST, &path, Some(&body))
        .await
        .map_err(api)?;
    let id = created["id"]
        .as_str()
        .or(body["id"].as_str())
        .unwrap_or_default();
    writeln!(io.out, "Created {noun} {id}.").map_err(|e| e.to_string())
}

/// `porch delete`.
pub(crate) async fn delete(session: &Session, what: Delete, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let org = format!("/orgs/{}", segment(session.org()?));
    let (path, body, noun, id) = match what {
        Delete::App { id } => (
            format!("{org}/apps/{}", segment(&id)),
            None,
            "application",
            id,
        ),
        Delete::Env { id } => {
            let path = format!(
                "{org}/apps/{}/envs/{}",
                segment(session.app()?),
                segment(&id)
            );
            // The deletion is checked against the generation just read.
            let env = client
                .request(Method::GET, &path, None)
                .await
                .map_err(api)?;
            let body = json!({"expected_generation": env["generation"]});
            (path, Some(body), "environment", id)
        }
        Delete::EnvType { id } => (
            format!("{org}/env-types/{}", segment(&id)),
            None,
            "environment type",
            id,
        ),
        Delete::ResourceDefinition { id } => (
            format!("{org}/resources/defs/{}", segment(&id)),
            None,
            "resource definition",
            id,
        ),
        Delete::ResourceType { id } => (
            format!("{org}/resources/types/{}", segment(&id)),
            None,
            "resource type",
            id,
        ),
        Delete::CloudAccount { id } => (
            format!("{org}/resources/accounts/{}", segment(&id)),
            None,
            "cloud account",
            id,
        ),
    };
    let (status, value) = client
        .send(Method::DELETE, &path, body.as_ref(), &[])
        .await
        .map_err(api)?;
    if status.as_u16() == 202 {
        writeln!(
            io.out,
            "Deletion of {noun} {id} accepted; its resources are being deprovisioned."
        )
        .map_err(|e| e.to_string())?;
        if !value.is_null() {
            output::print(io.out, Format::Yaml, &value, &[])?;
        }
        return Ok(());
    }
    writeln!(io.out, "Deleted {noun} {id}.").map_err(|e| e.to_string())
}
