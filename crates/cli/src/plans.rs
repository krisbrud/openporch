//! Previews and acknowledgements: `porch workload plan`, `--preview`, and the
//! `--plan`/`--ack` pair that turns a preview's marked changes into permits.
//!
//! A destructive change (a deletion, a replacement, a change whose effect is
//! unknown) runs only with a permit issued from a preview. The CLI asks for
//! each permit only when the command names the change with `--ack`, as the
//! preview showed it; there is no flag that acknowledges everything at once.
//! The deployment then runs at the preview's environment generation, so a
//! preview made stale by a later change is refused by the server.

use clap::Args;
use openporch_client::{Client, Error as ApiError, Method, segment};
use serde_json::{Value, json};

use crate::Io;

/// The error code of a destructive change without a permit.
pub(crate) const APPROVAL_REQUIRED: &str = "DPL-001";
/// The error code of an unknown, expired, stale or used permit.
pub(crate) const PERMIT_INVALID: &str = "DPL-003";
/// The error code of a conflict, such as a stale generation.
pub(crate) const CONFLICT: &str = "API-004";
/// The error code of an acceptance blocked by resources needing recovery.
pub(crate) const RECOVERY_REQUIRED: &str = "DPL-004";

/// A change named for a permit: `DESCRIPTOR=ACTION`, with `@FINGERPRINT` for
/// the driver plan of a replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    /// The resource, as the preview names it.
    pub descriptor: String,
    /// The preview's action: `delete`, `replace`, `unknown` or `detach`.
    pub action: String,
    /// The driver plan a replacement covers.
    pub fingerprint: Option<String>,
}

pub(crate) fn ack(s: &str) -> Result<Ack, String> {
    // A fingerprint may contain `=`, so it is split off first.
    let (named, fingerprint) = match s.split_once('@') {
        Some((n, f)) if !f.is_empty() => (n, Some(f.to_owned())),
        Some(_) => return Err("expected a fingerprint after `@`".into()),
        None => (s, None),
    };
    let (descriptor, action) = named
        .rsplit_once('=')
        .filter(|(d, a)| !d.is_empty() && !a.is_empty())
        .ok_or("expected DESCRIPTOR=ACTION")?;
    Ok(Ack {
        descriptor: descriptor.to_owned(),
        action: action.to_owned(),
        fingerprint,
    })
}

/// How a destructive command is confirmed.
#[derive(Debug, Args, Default)]
pub struct Confirm {
    /// Preview the change and print it, without changing anything.
    #[arg(long, conflicts_with = "plan")]
    pub preview: bool,
    /// The preview this command carries out: it runs at the preview's
    /// environment generation, with a permit for each `--ack`.
    #[arg(long, value_name = "PLAN")]
    pub plan: Option<String>,
    /// Acknowledges a change the preview marks as needing a permit, as the
    /// preview names it: DESCRIPTOR=ACTION, with @FINGERPRINT for a
    /// replacement's driver plan.
    #[arg(long = "ack", value_name = "DESCRIPTOR=ACTION", value_parser = ack, requires = "plan")]
    pub acks: Vec<Ack>,
}

/// A preview's acknowledged changes.
#[derive(Debug)]
pub(crate) struct Acknowledged {
    /// The preview.
    pub id: String,
    /// The environment generation it was made at.
    pub base_generation: i64,
    /// The permits issued for the acknowledged changes.
    pub permits: Vec<String>,
}

/// What `--ack` must name for each change in `plan` that needs a permit,
/// with what is asked of the server for those `acks` name.
#[derive(Debug)]
struct Checked {
    asks: Vec<Value>,
    missing: Vec<String>,
}

/// The `--ack` value for a preview entry.
fn ack_for(entry: &Value) -> String {
    let descriptor = entry["descriptor"].as_str().unwrap_or_default();
    let action = entry["action"].as_str().unwrap_or_default();
    match entry["driver_plan"]["fingerprint"].as_str() {
        Some(f) => format!("{descriptor}={action}@{f}"),
        None => format!("{descriptor}={action}"),
    }
}

fn needs_permit(entry: &Value) -> bool {
    entry["permit_required"] == true
}

fn entries(plan: &Value) -> &[Value] {
    plan["resources"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Matches `acks` against the changes of `plan` that need a permit. An ack the
/// preview does not mark, or names differently, is an error.
fn check(plan: &Value, acks: &[&Ack]) -> Result<Checked, String> {
    let id = plan["id"].as_str().unwrap_or_default();
    let mut checked = Checked {
        asks: Vec::new(),
        missing: Vec::new(),
    };
    for a in acks {
        let entry = entries(plan)
            .iter()
            .find(|e| e["descriptor"] == a.descriptor.as_str())
            .filter(|e| needs_permit(e))
            .ok_or_else(|| {
                format!(
                    "preview {id} has no change to `{}` that needs acknowledging",
                    a.descriptor
                )
            })?;
        let action = entry["action"].as_str().unwrap_or_default();
        let fingerprint = entry["driver_plan"]["fingerprint"].as_str();
        if a.action != action || a.fingerprint.as_deref() != fingerprint {
            return Err(format!(
                "preview {id} shows `{}` as `--ack {}`, not `{}={}{}`",
                a.descriptor,
                ack_for(entry),
                a.descriptor,
                a.action,
                a.fingerprint
                    .as_deref()
                    .map(|f| format!("@{f}"))
                    .unwrap_or_default()
            ));
        }
        checked.asks.push(json!({
            "descriptor": a.descriptor,
            "action": a.action,
            "fingerprint": a.fingerprint,
        }));
    }
    for entry in entries(plan).iter().filter(|e| needs_permit(e)) {
        if !acks
            .iter()
            .any(|a| entry["descriptor"] == a.descriptor.as_str())
        {
            checked.missing.push(ack_for(entry));
        }
    }
    Ok(checked)
}

/// `--ack` arguments for `missing`.
pub(crate) fn ack_args(missing: &[String]) -> String {
    missing.iter().map(|m| format!(" --ack '{m}'")).collect()
}

/// Creates a preview at `env_path` (an environment's API path).
pub(crate) async fn create(client: &Client, env_path: &str, body: &Value) -> Result<Value, String> {
    client
        .request(Method::POST, &format!("{env_path}/plans"), Some(body))
        .await
        .map_err(crate::api)
}

/// The `--ack` values for the changes of `plan` that need a permit.
pub(crate) fn required(plan: &Value) -> Vec<String> {
    entries(plan)
        .iter()
        .filter(|e| needs_permit(e))
        .map(ack_for)
        .collect()
}

/// Prints `plan` of `scope` (`app/env`): each change, what is unknown and
/// what it needs: `permission`, or the deploy permission. When changes need permits and `command` is given, it ends
/// with `command` and the `--plan` and `--ack` that carry the preview out.
pub(crate) fn print(
    io: &mut Io<'_>,
    plan: &Value,
    scope: &str,
    permission: Option<&str>,
    command: Option<&str>,
) -> Result<(), String> {
    let id = plan["id"].as_str().unwrap_or_default();
    let mut text = format!(
        "Preview {id} of {scope} at generation {}:\n",
        plan["base_generation"]
    );
    let list = entries(plan);
    if list.is_empty() {
        text.push_str("  no changes\n");
    }
    for e in list {
        let mut action = e["action"].as_str().unwrap_or("?").to_owned();
        if e["known"] == false && action != "unknown" {
            action.push('?');
        }
        text.push_str(&format!(
            "  {action:<8} {}",
            e["descriptor"].as_str().unwrap_or_default()
        ));
        if let Some(reason) = e["reason"].as_str().filter(|r| !r.is_empty()) {
            text.push_str(&format!(": {reason}"));
        }
        if needs_permit(e) {
            text.push_str(" [permit required]");
        }
        text.push('\n');
    }
    let unknown: Vec<&str> = list
        .iter()
        .filter(|e| e["action"] == "unknown" || e["known"] == false)
        .filter_map(|e| e["descriptor"].as_str())
        .collect();
    if !unknown.is_empty() {
        text.push_str(&format!(
            "Not known until the drivers run: {}. A preview is never a complete infrastructure plan.\n",
            unknown.join(", ")
        ));
    }
    let required = required(plan);
    let permission = match permission {
        Some(p) => p.to_owned(),
        None if plan["kind"] == "detach" => format!("an administrator, for {scope}"),
        None => format!("the deploy permission on {scope}"),
    };
    text.push_str(&format!("Needs {permission}"));
    match command {
        _ if required.is_empty() => text.push_str(".\n"),
        Some(command) => text.push_str(&format!(
            ", and a permit for each change marked above:\n  {command} --plan {id}{}\n",
            ack_args(&required)
        )),
        None => text.push_str(", and a permit for each change marked above.\n"),
    }
    io.out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// A preview whose acknowledgements were checked, before permits are issued.
pub(crate) struct Prepared {
    path: String,
    id: String,
    /// The environment generation the preview was made at.
    pub base_generation: i64,
    asks: Vec<Value>,
    /// The `--ack` values still missing.
    pub missing: Vec<String>,
}

/// Fetches preview `plan_id` of `kind` at `env_path` and checks `acks`
/// against the changes it marks, without issuing anything.
pub(crate) async fn prepare(
    client: &Client,
    env_path: &str,
    plan_id: &str,
    kind: &str,
    acks: &[&Ack],
) -> Result<Prepared, String> {
    let path = format!("{env_path}/plans/{}", segment(plan_id));
    let plan = client
        .request(Method::GET, &path, None)
        .await
        .map_err(crate::api)?;
    if plan["kind"] != kind {
        return Err(format!(
            "preview {plan_id} is a `{}` preview, not `{kind}`",
            plan["kind"].as_str().unwrap_or_default()
        ));
    }
    let base_generation = plan["base_generation"]
        .as_i64()
        .ok_or("unexpected response from the server: no preview generation")?;
    let checked = check(&plan, acks)?;
    Ok(Prepared {
        path,
        id: plan_id.to_owned(),
        base_generation,
        asks: checked.asks,
        missing: checked.missing,
    })
}

/// Issues the permits `prepared` asks for. `preview` makes a new preview.
pub(crate) async fn issue(
    client: &Client,
    prepared: Prepared,
    preview: &str,
) -> Result<Acknowledged, String> {
    let plan_id = &prepared.id;
    let permits = if prepared.asks.is_empty() {
        Vec::new()
    } else {
        let issued = client
            .request(
                Method::POST,
                &format!("{}/permits", prepared.path),
                Some(&json!({"permits": prepared.asks})),
            )
            .await
            .map_err(|e| match &e {
                ApiError::Api { code, .. } if code == PERMIT_INVALID => format!(
                    "{}\npreview {plan_id} no longer matches the environment, so nothing was changed; preview again:\n  {preview}",
                    crate::api(e)
                ),
                _ => crate::api(e),
            })?;
        issued["permits"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|p| p["id"].as_str().map(str::to_owned))
            .collect()
    };
    Ok(Acknowledged {
        id: prepared.id,
        base_generation: prepared.base_generation,
        permits,
    })
}

/// Fetches preview `plan_id` of `kind` at `env_path`, checks that `acks` name
/// every change it marks, and issues their permits. `command` carries out a
/// preview and `preview` makes a new one.
pub(crate) async fn acknowledge(
    client: &Client,
    env_path: &str,
    plan_id: &str,
    kind: &str,
    acks: &[&Ack],
    command: &str,
    preview: &str,
) -> Result<Acknowledged, String> {
    let prepared = prepare(client, env_path, plan_id, kind, acks).await?;
    if !prepared.missing.is_empty() {
        return Err(format!(
            "preview {plan_id} has changes that need acknowledging, one by one:\n  {command} --plan {plan_id}{}",
            ack_args(&prepared.missing)
        ));
    }
    issue(client, prepared, preview).await
}

/// The message for a refused acceptance: a stale preview, a missing permit or
/// resources needing recovery each say what to do next. `current` is the
/// environment's generation after the refusal, when it could be read.
pub(crate) fn refused(
    e: ApiError,
    acknowledged: Option<&Acknowledged>,
    current: Option<i64>,
    preview: &str,
) -> String {
    let code = match &e {
        ApiError::Api { code, .. } => code.clone(),
        _ => String::new(),
    };
    let message = crate::api(e);
    // A generation conflict is `API-004`, which other conflicts share, so it
    // counts as a stale preview only when the generation did move.
    let conflict = code == PERMIT_INVALID
        || code == APPROVAL_REQUIRED
        || (code == CONFLICT
            && current.is_some_and(|g| acknowledged.is_some_and(|a| g != a.base_generation)));
    if let Some(a) = acknowledged
        && conflict
    {
        return format!(
            "{message}\npreview {} no longer matches the environment, so nothing was changed; preview again:\n  {preview}",
            a.id
        );
    }
    match code.as_str() {
        APPROVAL_REQUIRED | PERMIT_INVALID => format!(
            "{message}\nnothing was changed; preview the change, then acknowledge what it marks:\n  {preview}"
        ),
        RECOVERY_REQUIRED => format!(
            "{message}\nnothing was changed; recover those resources first with `porch resources recover <type> <res_id>`"
        ),
        _ => message,
    }
}

/// `porch resources recover` and `resolve` take a resource as `<type>` and
/// `<res_id>`; a descriptor is `<type>.<class>#<res_id>`.
pub(crate) fn type_and_id(descriptor: &str) -> Option<(&str, &str)> {
    let (ty_class, res_id) = descriptor.split_once('#')?;
    let ty = ty_class.split_once('.').map_or(ty_class, |(t, _)| t);
    Some((ty, res_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acks_name_a_change_and_optionally_its_driver_plan() {
        assert_eq!(
            ack("postgres.default#modules.web.externals.db=delete").unwrap(),
            Ack {
                descriptor: "postgres.default#modules.web.externals.db".into(),
                action: "delete".into(),
                fingerprint: None,
            }
        );
        assert_eq!(
            ack("a.b#c=replace@f00").unwrap().fingerprint.as_deref(),
            Some("f00")
        );
        for bad in ["a.b#c", "=delete", "a.b#c=", "a.b#c=replace@"] {
            assert!(ack(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_marked_change_must_be_acknowledged_as_shown() {
        let plan = json!({"id": "pln-1", "resources": [
            {"descriptor": "a.default#x", "action": "delete", "permit_required": true},
            {"descriptor": "b.default#y", "action": "replace", "permit_required": true,
             "driver_plan": {"fingerprint": "f1"}},
            {"descriptor": "c.default#z", "action": "update", "permit_required": false},
        ]});
        let x = ack("a.default#x=delete").unwrap();
        let checked = check(&plan, &[&x]).unwrap();
        assert_eq!(checked.missing, ["b.default#y=replace@f1"]);

        // A replacement is acknowledged with its driver plan.
        let y = ack("b.default#y=replace").unwrap();
        assert!(check(&plan, &[&x, &y]).unwrap_err().contains("replace@f1"));
        let y = ack("b.default#y=replace@f1").unwrap();
        let checked = check(&plan, &[&x, &y]).unwrap();
        assert!(checked.missing.is_empty());
        assert_eq!(checked.asks.len(), 2);

        // Nothing is acknowledged that the preview does not mark.
        let z = ack("c.default#z=delete").unwrap();
        assert!(check(&plan, &[&z]).is_err());
        let wrong = ack("a.default#x=replace").unwrap();
        assert!(check(&plan, &[&wrong]).is_err());
    }

    #[test]
    fn descriptors_split_into_type_and_resource_id() {
        assert_eq!(
            type_and_id("postgres.default#modules.web.externals.db"),
            Some(("postgres", "modules.web.externals.db"))
        );
        assert_eq!(type_and_id("nope"), None);
    }
}
