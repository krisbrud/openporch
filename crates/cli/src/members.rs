//! `porch users`, `porch apps users`, `porch env-types users`, `porch secretstores`
//! `porch audit-logs` and `porch expire-audit-logs`. The server applies the same permission rules as for the
//! API; the CLI only shapes requests and output.

use clap::{Args, Subcommand};
use openporch_client::{Method, segment, with_query};
use serde_json::{Value, json};

use crate::objects::{FileArg, document_body, read_document};
use crate::output::{self, Format, timestamp};
use crate::{Io, Session, api};

/// The output format flag.
#[derive(Debug, Args)]
pub struct OutputArg {
    /// The output format.
    #[arg(short, long, value_enum, default_value_t)]
    pub output: Format,
}

/// `porch users`.
#[derive(Debug, Subcommand)]
pub enum Users {
    /// Lists the organization's members.
    List(OutputArg),
    /// Invites a person, by email or GitHub login. Share the printed code with them.
    Invite {
        /// Their organization role.
        #[arg(long)]
        role: String,
        /// Their verified email address.
        #[arg(
            long,
            conflicts_with = "github_login",
            required_unless_present = "github_login"
        )]
        email: Option<String>,
        /// Their GitHub login.
        #[arg(long, value_name = "LOGIN")]
        github_login: Option<String>,
    },
    /// Creates a service user, such as one for CI.
    CreateService {
        /// The user ID.
        id: String,
        /// Its organization role.
        #[arg(long, default_value = "member")]
        role: String,
        /// A display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Changes a member's organization role.
    SetRole {
        /// The user ID.
        id: String,
        /// The new role.
        role: String,
    },
    /// Removes a member, revoking their tokens, sessions and roles.
    Remove {
        /// The user ID.
        id: String,
    },
    /// Manages a user's API tokens.
    #[command(subcommand)]
    Tokens(Tokens),
}

/// `porch users tokens`.
#[derive(Debug, Subcommand)]
pub enum Tokens {
    /// Lists a user's tokens, without their values.
    List {
        /// The user ID.
        user: String,
        #[command(flatten)]
        output: OutputArg,
    },
    /// Creates a token and prints its value, once.
    Create {
        /// The user ID.
        user: String,
        /// The token ID.
        #[arg(long)]
        id: String,
        /// A description.
        #[arg(long)]
        description: Option<String>,
        /// How long the token is valid, such as `90d`, `12h` or `30m`.
        #[arg(long, value_name = "DURATION")]
        expires_in: Option<String>,
    },
    /// Revokes a token.
    Revoke {
        /// The user ID.
        user: String,
        /// The token ID.
        id: String,
    },
}

/// `porch apps`.
#[derive(Debug, Subcommand)]
pub enum Apps {
    /// Manages an application's users.
    Users {
        /// The application ID.
        app: String,
        #[command(subcommand)]
        action: AppUsers,
    },
}

/// `porch apps users <app>`.
#[derive(Debug, Subcommand)]
pub enum AppUsers {
    /// Lists the application's users and their roles.
    List(OutputArg),
    /// Gives a user a role (`owner`, `developer` or `viewer`), replacing any other.
    Grant {
        /// The user ID.
        user: String,
        /// The role.
        role: String,
    },
    /// Removes a user's role.
    Revoke {
        /// The user ID.
        user: String,
    },
}

/// `porch env-types`.
#[derive(Debug, Subcommand)]
pub enum EnvTypes {
    /// Manages who may deploy to an environment type.
    Users {
        /// The environment type ID.
        env_type: String,
        #[command(subcommand)]
        action: EnvTypeUsers,
    },
}

/// `porch env-types users <type>`.
#[derive(Debug, Subcommand)]
pub enum EnvTypeUsers {
    /// Lists the environment type's deployers.
    List(OutputArg),
    /// Gives a user the `deployer` role.
    Grant {
        /// The user ID.
        user: String,
    },
    /// Removes a user's `deployer` role.
    Revoke {
        /// The user ID.
        user: String,
    },
}

/// `porch secretstores`.
#[derive(Debug, Subcommand)]
pub enum SecretStores {
    /// Lists secret stores.
    List(OutputArg),
    /// Shows a secret store.
    Get {
        /// The store ID.
        id: String,
        #[command(flatten)]
        output: OutputArg,
    },
    /// Creates a secret store from a `kind: SecretStore` YAML document.
    Create(FileArg),
    /// Deletes an unused secret store.
    Delete {
        /// The store ID.
        id: String,
    },
    /// Lists the grants that let applications use a store's secrets.
    Grants {
        /// The store ID.
        id: String,
        #[command(flatten)]
        output: OutputArg,
    },
}

/// `porch audit-logs`.
#[derive(Debug, Args)]
pub struct AuditLogs {
    /// Only this principal's entries.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,
    /// Only paths starting with this, such as `/orgs/acme/apps/shop`.
    #[arg(long, value_name = "PREFIX")]
    pub path_prefix: Option<String>,
    /// Only this HTTP method.
    #[arg(long)]
    pub method: Option<String>,
    /// Entries from this time on, in Unix milliseconds.
    #[arg(long, value_name = "MS")]
    pub from: Option<i64>,
    /// Entries before this time, in Unix milliseconds.
    #[arg(long, value_name = "MS")]
    pub to: Option<i64>,
    /// At most this many entries.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Entries older than this sequence number, to page back.
    #[arg(long, value_name = "SEQ")]
    pub before: Option<i64>,
    /// Every matching entry, following the pages, for export.
    #[arg(long)]
    pub all: bool,
    #[command(flatten)]
    pub output: OutputArg,
}

/// `porch expire-audit-logs`.
#[derive(Debug, Args)]
pub struct ExpireAuditLogs {
    /// Report how many entries would expire without removing any.
    #[arg(long)]
    pub dry_run: bool,
    /// Expire only entries written before this time, in Unix milliseconds,
    /// such as the `before` of a dry run whose entries were exported.
    #[arg(long, value_name = "MS")]
    pub before: Option<i64>,
    #[command(flatten)]
    pub output: OutputArg,
}

/// Parses `90d`, `12h`, `30m` or `2w` into milliseconds.
pub fn duration_ms(text: &str) -> Result<i64, String> {
    let invalid = || format!("`{text}` is not a duration such as 90d, 12h or 30m");
    let text = text.trim();
    let split = text.len().checked_sub(1).ok_or_else(invalid)?;
    let (number, unit) = text.split_at(split);
    let n: i64 = number.parse().map_err(|_| invalid())?;
    let unit_ms: i64 = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return Err(invalid()),
    };
    match n.checked_mul(unit_ms) {
        Some(ms) if n > 0 => Ok(ms),
        _ => Err(invalid()),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn org(session: &Session) -> Result<String, String> {
    Ok(format!("/orgs/{}", segment(session.org()?)))
}

fn say(io: &mut Io<'_>, text: &str) -> Result<(), String> {
    writeln!(io.out, "{text}").map_err(|e| e.to_string())
}

/// `porch users`.
pub(crate) async fn users(session: &Session, cmd: Users, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let base = format!("{}/users", org(session)?);
    match cmd {
        Users::List(o) => {
            let list = client
                .request(Method::GET, &base, None)
                .await
                .map_err(api)?;
            output::print(
                io.out,
                o.output,
                &list,
                &[
                    ("ID", "id"),
                    ("TYPE", "type"),
                    ("ROLE", "role"),
                    ("NAME", "name"),
                ],
            )
        }
        Users::Invite {
            role,
            email,
            github_login,
        } => {
            let mut body = json!({"role": role});
            if let Some(email) = email {
                body["email"] = json!(email);
            }
            if let Some(login) = github_login {
                body["provider"] = json!("github");
                body["login"] = json!(login);
            }
            let path = format!("{}/invitations", org(session)?);
            let inv = client
                .request(Method::POST, &path, Some(&body))
                .await
                .map_err(api)?;
            let code = inv["code"].as_str().unwrap_or_default();
            let expires = inv["expires_at"]
                .as_i64()
                .map(timestamp)
                .unwrap_or_default();
            say(
                io,
                &format!(
                    "Invitation {} created; it expires {expires}.\nShare this code with them; it is shown once:\n{code}",
                    inv["id"].as_str().unwrap_or_default()
                ),
            )
        }
        Users::CreateService { id, role, name } => {
            let mut body = json!({"id": id, "type": "service", "role": role});
            if let Some(name) = name {
                body["name"] = json!(name);
            }
            client
                .request(Method::POST, &base, Some(&body))
                .await
                .map_err(api)?;
            say(
                io,
                &format!(
                    "Created service user {id}. Create a token with `porch users tokens create {id} --id <ID>`."
                ),
            )
        }
        Users::SetRole { id, role } => {
            let path = format!("{base}/{}", segment(&id));
            client
                .request(Method::PATCH, &path, Some(&json!({"role": role})))
                .await
                .map_err(api)?;
            say(io, &format!("{id} is now {role}."))
        }
        Users::Remove { id } => {
            let path = format!("{base}/{}", segment(&id));
            client
                .request(Method::DELETE, &path, None)
                .await
                .map_err(api)?;
            say(io, &format!("Removed {id}."))
        }
        Users::Tokens(t) => tokens(session, t, io).await,
    }
}

async fn tokens(session: &Session, cmd: Tokens, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let base = |user: &str| -> Result<String, String> {
        Ok(format!("{}/users/{}/tokens", org(session)?, segment(user)))
    };
    match cmd {
        Tokens::List { user, output } => {
            let list = client
                .request(Method::GET, &base(&user)?, None)
                .await
                .map_err(api)?;
            output::print(
                io.out,
                output.output,
                &list,
                &[
                    ("ID", "id"),
                    ("DESCRIPTION", "description"),
                    ("EXPIRES", "expires_at"),
                    ("LAST USED", "last_used_at"),
                    ("REVOKED", "revoked_at"),
                ],
            )
        }
        Tokens::Create {
            user,
            id,
            description,
            expires_in,
        } => {
            let mut body = json!({"id": id});
            if let Some(d) = description {
                body["description"] = json!(d);
            }
            if let Some(d) = expires_in {
                let at = now_ms()
                    .checked_add(duration_ms(&d)?)
                    .ok_or("the expiry is too far in the future")?;
                body["expires_at"] = json!(at);
            }
            let created = client
                .request(Method::POST, &base(&user)?, Some(&body))
                .await
                .map_err(api)?;
            let token = created["token"]
                .as_str()
                .ok_or("the server returned no token")?;
            let expiry = match created["expires_at"].as_i64() {
                Some(ms) => format!("expires {}", timestamp(ms)),
                None => "never expires".to_owned(),
            };
            // The value goes alone on standard output, so it can be piped into a
            // secret store; the notes go to standard error.
            writeln!(
                io.err,
                "Token {id} for {user} {expiry}. It is shown once; store it now."
            )
            .map_err(|e| e.to_string())?;
            say(io, token)
        }
        Tokens::Revoke { user, id } => {
            let path = format!("{}/{}", base(&user)?, segment(&id));
            client
                .request(Method::DELETE, &path, None)
                .await
                .map_err(api)?;
            say(io, &format!("Revoked token {id} of {user}."))
        }
    }
}

/// `porch apps`.
pub(crate) async fn apps(session: &Session, cmd: Apps, io: &mut Io<'_>) -> Result<(), String> {
    let client = session.client()?;
    let Apps::Users { app, action } = cmd;
    let base = format!("{}/apps/{}/users", org(session)?, segment(&app));
    match action {
        AppUsers::List(o) => {
            let list = client
                .request(Method::GET, &base, None)
                .await
                .map_err(api)?;
            output::print(
                io.out,
                o.output,
                &list,
                &[("USER", "user"), ("ROLE", "role")],
            )
        }
        AppUsers::Grant { user, role } => {
            // A new user is added; one who already has a role changes it.
            let added = client
                .request(
                    Method::POST,
                    &base,
                    Some(&json!({"id": user, "role": role})),
                )
                .await;
            match added {
                Err(e) if e.status() == Some(409) => {
                    let path = format!("{base}/{}", segment(&user));
                    client
                        .request(Method::PATCH, &path, Some(&json!({"role": role})))
                        .await
                        .map_err(api)?;
                }
                other => {
                    other.map_err(api)?;
                }
            }
            say(io, &format!("{user} is {role} on {app}."))
        }
        AppUsers::Revoke { user } => {
            let path = format!("{base}/{}", segment(&user));
            client
                .request(Method::DELETE, &path, None)
                .await
                .map_err(api)?;
            say(io, &format!("{user} has no role on {app} any more."))
        }
    }
}

/// `porch env-types`.
pub(crate) async fn env_types(
    session: &Session,
    cmd: EnvTypes,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let client = session.client()?;
    let EnvTypes::Users { env_type, action } = cmd;
    let base = format!("{}/env-types/{}/users", org(session)?, segment(&env_type));
    match action {
        EnvTypeUsers::List(o) => {
            let list = client
                .request(Method::GET, &base, None)
                .await
                .map_err(api)?;
            output::print(
                io.out,
                o.output,
                &list,
                &[("USER", "user"), ("ROLE", "role")],
            )
        }
        EnvTypeUsers::Grant { user } => {
            client
                .request(
                    Method::POST,
                    &base,
                    Some(&json!({"id": user, "role": "deployer"})),
                )
                .await
                .map_err(api)?;
            say(
                io,
                &format!("{user} may deploy to {env_type} environments."),
            )
        }
        EnvTypeUsers::Revoke { user } => {
            let path = format!("{base}/{}", segment(&user));
            client
                .request(Method::DELETE, &path, None)
                .await
                .map_err(api)?;
            say(
                io,
                &format!("{user} may no longer deploy to {env_type} environments."),
            )
        }
    }
}

/// `porch secretstores`.
pub(crate) async fn secret_stores(
    session: &Session,
    cmd: SecretStores,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let client = session.client()?;
    let base = format!("{}/secretstores", org(session)?);
    let columns = [("ID", "id"), ("TYPE", "type"), ("PRIMARY", "primary")];
    match cmd {
        SecretStores::List(o) => {
            let list = client
                .request(Method::GET, &base, None)
                .await
                .map_err(api)?;
            output::print(io.out, o.output, &list, &columns)
        }
        SecretStores::Get { id, output } => {
            let path = format!("{base}/{}", segment(&id));
            let store = client
                .request(Method::GET, &path, None)
                .await
                .map_err(api)?;
            output::print(io.out, output.output, &store, &columns)
        }
        SecretStores::Create(f) => {
            let body = document_body(&read_document(&f.file, io)?, "SecretStore")?;
            client
                .request(Method::POST, &base, Some(&body))
                .await
                .map_err(api)?;
            say(
                io,
                &format!(
                    "Created secret store {}.",
                    body["id"].as_str().unwrap_or_default()
                ),
            )
        }
        SecretStores::Delete { id } => {
            let path = format!("{base}/{}", segment(&id));
            client
                .request(Method::DELETE, &path, None)
                .await
                .map_err(api)?;
            say(io, &format!("Deleted secret store {id}."))
        }
        SecretStores::Grants { id, output } => {
            let path = format!("{base}/{}/grants", segment(&id));
            let list = client
                .request(Method::GET, &path, None)
                .await
                .map_err(api)?;
            output::print(
                io.out,
                output.output,
                &list,
                &[
                    ("ID", "id"),
                    ("REF", "ref"),
                    ("APP", "app_id"),
                    ("ENV", "env_id"),
                    ("ORG WIDE", "org_wide"),
                ],
            )
        }
    }
}

/// `porch audit-logs`.
pub(crate) async fn audit_logs(
    session: &Session,
    args: AuditLogs,
    io: &mut Io<'_>,
) -> Result<(), String> {
    if args.all && args.output.output != Format::Json {
        return Err("`--all` exports JSON: add `-o json`".into());
    }
    let client = session.client()?;
    let mut before = args.before;
    let mut first = true;
    loop {
        let path = with_query(
            &format!("{}/audit-logs", org(session)?),
            &[
                ("user_id", args.user.clone()),
                ("path_prefix", args.path_prefix.clone()),
                (
                    "method",
                    args.method.as_ref().map(|m| m.to_ascii_uppercase()),
                ),
                ("from", args.from.map(|v| v.to_string())),
                ("to", args.to.map(|v| v.to_string())),
                ("limit", args.limit.map(|v| v.to_string())),
                ("before", before.map(|v| v.to_string())),
            ],
        );
        let page: Value = client
            .request(Method::GET, &path, None)
            .await
            .map_err(api)?;
        let next = page["next"].as_i64();
        if !args.all {
            output::print(
                io.out,
                args.output.output,
                &page,
                &[
                    ("SEQ", "seq"),
                    ("AT", "at"),
                    ("USER", "user_id"),
                    ("METHOD", "method"),
                    ("PATH", "path"),
                    ("ACTION", "action"),
                    ("OUTCOME", "outcome"),
                ],
            )?;
            if args.output.output == Format::Table
                && let Some(next) = next
            {
                writeln!(
                    io.err,
                    "More entries: add `--before {next}`, or `--all -o json` for every one"
                )
                .map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        // Each page is written as it arrives, so an export of any size
        // needs only one page in memory. An export cut short by an error
        // is not valid JSON, and the command fails.
        let write = |out: &mut dyn std::io::Write, text: &str| {
            out.write_all(text.as_bytes()).map_err(|e| e.to_string())
        };
        if first {
            write(io.out, "{\"entries\": [")?;
        }
        for entry in page["entries"].as_array().into_iter().flatten() {
            let text = serde_json::to_string(entry).map_err(|e| e.to_string())?;
            write(io.out, if first { "\n" } else { ",\n" })?;
            write(io.out, &text)?;
            first = false;
        }
        match next {
            Some(seq) => before = Some(seq),
            None => return write(io.out, "\n]}\n"),
        }
    }
}

pub(crate) async fn expire_audit_logs(
    session: &Session,
    args: ExpireAuditLogs,
    io: &mut Io<'_>,
) -> Result<(), String> {
    let result: Value = session
        .client()?
        .request(
            Method::POST,
            &format!("{}/audit-logs/expire", org(session)?),
            Some(&json!({ "dry_run": args.dry_run, "before": args.before })),
        )
        .await
        .map_err(api)?;
    if args.output.output == Format::Table {
        let expired = result["expired"].as_i64().unwrap_or(0);
        let remaining = result["remaining"].as_i64().unwrap_or(0);
        let days = result["retention_days"].as_i64().unwrap_or(0);
        let verb = if args.dry_run {
            "would expire"
        } else {
            "expired"
        };
        let retained = &result["retained"];
        let drivers: Vec<String> = retained["by_driver"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(driver, n)| format!("{n} {driver}"))
            .collect();
        writeln!(
            io.out,
            "{expired} entries older than {days} days {verb}; {remaining} kept\n\
             Kept: {} resources ({}), {} needing recovery, {} awaiting cleanup\n{}",
            retained["resources"].as_i64().unwrap_or(0),
            if drivers.is_empty() {
                "none".to_owned()
            } else {
                drivers.join(", ")
            },
            retained["needing_recovery"].as_i64().unwrap_or(0),
            retained["cleanup_pending"].as_i64().unwrap_or(0),
            result["note"].as_str().unwrap_or_default(),
        )
        .map_err(|e| e.to_string())
    } else {
        output::print(io.out, args.output.output, &result, &[])
    }
}
