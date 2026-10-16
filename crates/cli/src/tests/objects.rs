//! Object, member and audit-log commands against a dev server: `Apply a definition
//! file` and `Create CI token` in `cli`, and the lifecycle verbs the API offers.

use serde_json::{Value, json};

use super::{DevServer, Machine, assert_no_secret};
use crate::Token;
use crate::objects::document_body;
use openporch_client::Client;

/// A machine logged in to `server` as its administrator, through the environment.
pub(super) fn admin_machine(server: &DevServer) -> Machine {
    let mut m = Machine::new();
    m.set("OPENPORCH_URL", &server.url)
        .set("OPENPORCH_ORG", "acme")
        .set("OPENPORCH_TOKEN", &server.admin);
    m
}

pub(super) fn admin_client(server: &DevServer) -> Client {
    Client::new(&server.url, "acme", Token::new(server.admin.clone())).unwrap()
}

const STG_REDIS: &str = r#"
apiVersion: openporch.dev/v1
kind: Definition
metadata:
  id: stg-redis
  name: Staging Redis
entity:
  type: redis
  driver_type: openporch/opentofu
  driver_inputs:
    values:
      source:
        url: https://example.com/redis.git
    secrets:
      source:
        password: hunter2-redis
  criteria:
    - env_type: staging
"#;

/// `Apply a definition file` in `cli`.
#[tokio::test]
async fn apply_a_definition_file() {
    let server = DevServer::start(None).await;
    let m = admin_machine(&server);
    let file = m.home.path().join("stg-redis.yaml");
    std::fs::write(&file, STG_REDIS).unwrap();

    let r = m
        .porch(
            &[
                "create",
                "resource-definition",
                "-f",
                file.to_str().unwrap(),
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(r.out, "Created resource definition stg-redis.\n");
    assert_no_secret(&r, "hunter2-redis");

    // The definition exists on the server, its secret stored as a reference.
    let def = admin_client(&server)
        .request(
            openporch_client::Method::GET,
            "/orgs/acme/resources/defs/stg-redis",
            None,
        )
        .await
        .unwrap();
    assert_eq!(def["name"], "Staging Redis");
    assert_eq!(def["type"], "redis");
    assert_eq!(def["driver_type"], "openporch/opentofu");
    assert_eq!(def["criteria"][0]["env_type"], "staging");
    assert_eq!(
        def["driver_inputs"]["secret_refs"]["source"]["password"]["store"],
        "openporch"
    );
    assert!(!def.to_string().contains("hunter2-redis"));

    let r = m.porch(&["get", "resource-definitions"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let row = r.out.lines().find(|l| l.starts_with("stg-redis")).unwrap();
    assert!(
        row.contains("redis") && row.contains("openporch/opentofu"),
        "{r:?}"
    );
    let r = m
        .porch(
            &["get", "resource-definition", "stg-redis", "-o", "yaml"],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let read: Value = serde_yaml_ng::from_str(&r.out).unwrap();
    assert_eq!(read["id"], "stg-redis");
    assert_no_secret(&r, "hunter2-redis");

    // From standard input too; a second create conflicts.
    let r = m
        .porch(&["create", "resource-definition", "-f", "-"], STG_REDIS)
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("API-004"), "{r:?}");
    assert_no_secret(&r, "hunter2-redis");

    // Broken documents are refused without quoting them.
    for (doc, message) in [
        (
            STG_REDIS.replace("kind: Definition", "kind: Workload"),
            "kind: Definition",
        ),
        (STG_REDIS.replace("openporch.dev/v1", "v9"), "apiVersion"),
        (STG_REDIS.replace("  id: stg-redis\n", ""), "metadata.id"),
        (
            STG_REDIS.replace("password: hunter2-redis", "password: [hunter2-redis"),
            "line",
        ),
    ] {
        let r = m
            .porch(&["create", "resource-definition", "-f", "-"], &doc)
            .await;
        assert_eq!(r.code, 1, "{r:?}");
        assert!(r.err.contains(message), "{message}: {r:?}");
        assert_no_secret(&r, "hunter2-redis");
    }

    // A manager is no administrator: the server's rule applies.
    let manager = server
        .post(
            "/orgs/acme/users",
            json!({"id": "carol", "type": "service", "role": "manager"}),
        )
        .await;
    assert_eq!(manager["id"], "carol");
    let token = server
        .post("/orgs/acme/users/carol/tokens", json!({"id": "t1"}))
        .await;
    let mut carol = admin_machine(&server);
    carol.set("OPENPORCH_TOKEN", token["token"].as_str().unwrap());
    let other = STG_REDIS.replace("id: stg-redis", "id: other");
    let r = carol
        .porch(&["create", "resource-definition", "-f", "-"], &other)
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("AUTH-002"), "{r:?}");

    let r = m
        .porch(&["delete", "resource-definition", "stg-redis"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m
        .porch(&["get", "resource-definition", "stg-redis"], "")
        .await;
    assert!(r.err.contains("API-001"), "{r:?}");
}

#[test]
fn documents_become_request_bodies() {
    let doc: Value = serde_yaml_ng::from_str(STG_REDIS).unwrap();
    let body = document_body(&doc, "Definition").unwrap();
    assert_eq!(body["id"], "stg-redis");
    assert_eq!(body["name"], "Staging Redis");
    assert_eq!(body["type"], "redis");
    assert!(body.get("metadata").is_none() && body.get("kind").is_none());
    // `entity.name` wins over `metadata.name`; `entity.id` is refused.
    let doc = json!({"kind": "Definition", "metadata": {"id": "a", "name": "m"},
                     "entity": {"name": "e"}});
    assert_eq!(document_body(&doc, "Definition").unwrap()["name"], "e");
    let doc = json!({"kind": "Definition", "metadata": {"id": "a"}, "entity": {"id": "b"}});
    assert!(document_body(&doc, "Definition").is_err());
    let doc = json!({"kind": "Definition", "metadata": {"id": "a"}, "entity": {}, "spec": {}});
    assert!(
        document_body(&doc, "Definition")
            .unwrap_err()
            .contains("spec")
    );
}

/// `Create CI token` in `cli`.
#[tokio::test]
async fn create_ci_token() {
    let server = DevServer::start(None).await;
    let m = admin_machine(&server);
    let r = m
        .porch(&["users", "create-service", "ci", "--role", "member"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");

    let before = crate::members::duration_ms("90d").unwrap();
    let r = m
        .porch(
            &[
                "users",
                "tokens",
                "create",
                "ci",
                "--id",
                "deploy",
                "--expires-in",
                "90d",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    // The value alone on standard output, the expiry next to it.
    let token = r.out.trim_end_matches('\n');
    assert_eq!(r.out.lines().count(), 1, "{r:?}");
    assert!(r.err.contains("Token deploy for ci expires 20"), "{r:?}");
    assert!(r.err.contains("shown once"), "{r:?}");

    // The token works, as `ci`, and expires in about 90 days.
    let ci = Client::new(&server.url, "acme", Token::new(token)).unwrap();
    assert_eq!(ci.current_user().await.unwrap()["id"], "ci");
    let r = m
        .porch(&["users", "tokens", "list", "ci", "-o", "json"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(!r.out.contains(token), "{r:?}");
    let list: Value = serde_json::from_str(&r.out).unwrap();
    let expires = list[0]["expires_at"].as_i64().unwrap();
    let created = list[0]["created_at"].as_i64().unwrap();
    assert!((expires - created - before).abs() < 60_000, "{list}");
    // The expiry shown is the one stored.
    let shown = crate::output::timestamp(expires);
    assert!(r.err.is_empty());
    let table = m.porch(&["users", "tokens", "list", "ci"], "").await;
    assert!(table.out.contains(&shown), "{table:?}");

    let r = m
        .porch(&["users", "tokens", "revoke", "ci", "deploy"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(ci.current_user().await.unwrap_err().status(), Some(401));

    for bad in ["90", "0d", "-1d", "90y", "d"] {
        let r = m
            .porch(
                &[
                    "users",
                    "tokens",
                    "create",
                    "ci",
                    "--id",
                    "x",
                    &format!("--expires-in={bad}"),
                ],
                "",
            )
            .await;
        assert_eq!(r.code, 1, "{bad}: {r:?}");
        assert!(r.err.contains("duration"), "{bad}: {r:?}");
    }
}

#[tokio::test]
async fn objects_have_the_lifecycle_verbs_of_the_api() {
    let server = DevServer::start(None).await;
    let mut m = admin_machine(&server);
    for args in [
        &["create", "app", "shop", "--name", "Shop"][..],
        &[
            "create",
            "env-type",
            "staging",
            "--description",
            "Pre-production",
        ][..],
        &["create", "env", "stg", "--type", "staging", "--app", "shop"][..],
    ] {
        let r = m.porch(args, "").await;
        assert_eq!(r.code, 0, "{args:?}: {r:?}");
        assert!(r.out.starts_with("Created "), "{r:?}");
    }
    let r = m.porch(&["get", "apps"], "").await;
    assert!(
        r.out
            .lines()
            .any(|l| l.starts_with("shop") && l.contains("Shop")),
        "{r:?}"
    );
    let r = m.porch(&["get", "env-types"], "").await;
    assert!(r.out.contains("Pre-production"), "{r:?}");

    // Commands scoped to an application read `--app` or the environment.
    let r = m.porch(&["get", "envs"], "").await;
    assert_eq!(r.code, 1);
    assert!(r.err.contains("OPENPORCH_APP"), "{r:?}");
    m.set("OPENPORCH_APP", "shop");
    let r = m.porch(&["get", "envs"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let stg = r.out.lines().find(|l| l.starts_with("stg")).unwrap();
    assert!(
        stg.contains("staging") && stg.contains("never_deployed"),
        "{r:?}"
    );
    assert!(r.out.lines().any(|l| l.starts_with("development")), "{r:?}");

    // Environment-scoped lists need `--env`.
    let r = m.porch(&["get", "deploys"], "").await;
    assert!(r.err.contains("OPENPORCH_ENV"), "{r:?}");
    for kind in ["deploys", "active-resources"] {
        let r = m.porch(&["get", kind, "--env", "stg"], "").await;
        assert_eq!(r.code, 0, "{kind}: {r:?}");
        assert_eq!(r.out.lines().count(), 1, "only the header: {r:?}");
    }
    for kind in [
        "deltas",
        "resource-types",
        "resource-classes",
        "cloud-accounts",
    ] {
        let r = m.porch(&["get", kind], "").await;
        assert_eq!(r.code, 0, "{kind}: {r:?}");
    }
    let r = m.porch(&["get", "resource-types"], "").await;
    assert!(r.out.lines().any(|l| l.starts_with("postgres")), "{r:?}");
    let r = m.porch(&["get", "sets"], "").await;
    assert!(r.err.contains("read by ID"), "{r:?}");
    let r = m.porch(&["get", "sets", "0"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let set: Value = serde_yaml_ng::from_str(&r.out).unwrap();
    assert!(set.is_object(), "{r:?}");

    // Sets and deployment records have no delete verb.
    for kind in ["set", "sets", "deploy", "deploys", "deployment"] {
        let r = m.porch(&["delete", kind, "x"], "").await;
        assert_eq!(r.code, 2, "{kind}: {r:?}");
    }

    let r = m
        .porch(&["get", "deploys", "--env", "stg", "--limit", "5"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m.porch(&["get", "apps", "--limit", "5"], "").await;
    assert!(r.err.contains("page the list of deploys"), "{r:?}");

    let client = admin_client(&server);
    let path = "/orgs/acme/apps/shop/envs/stg";
    let env = client
        .request(openporch_client::Method::GET, path, None)
        .await
        .unwrap();
    client
        .request(
            openporch_client::Method::DELETE,
            path,
            Some(&json!({"expected_generation": env["generation"]})),
        )
        .await
        .unwrap();
    let r = m.porch(&["delete", "env-type", "staging"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = m.porch(&["get", "env-type", "staging"], "").await;
    assert!(r.err.contains("API-001"), "{r:?}");
}

#[tokio::test]
async fn members_roles_secret_stores_and_audit_logs() {
    let server = DevServer::start(None).await;
    let m = admin_machine(&server);
    for args in [
        &["create", "app", "shop"][..],
        &["users", "create-service", "dev", "--name", "Developer"][..],
        &["apps", "users", "shop", "grant", "dev", "viewer"][..],
        &["apps", "users", "shop", "grant", "dev", "developer"][..],
        &["env-types", "users", "development", "grant", "dev"][..],
    ] {
        let r = m.porch(args, "").await;
        assert_eq!(r.code, 0, "{args:?}: {r:?}");
    }
    let r = m.porch(&["users", "list"], "").await;
    let dev = r.out.lines().find(|l| l.starts_with("dev ")).unwrap();
    assert!(dev.contains("service") && dev.contains("member"), "{r:?}");
    let r = m
        .porch(&["apps", "users", "shop", "list", "-o", "json"], "")
        .await;
    let users: Value = serde_json::from_str(&r.out).unwrap();
    assert!(
        users
            .as_array()
            .unwrap()
            .contains(&json!({"user": "dev", "role": "developer"})),
        "{users}"
    );
    let r = m
        .porch(&["env-types", "users", "development", "list"], "")
        .await;
    assert!(r.out.lines().any(|l| l.starts_with("dev")), "{r:?}");

    // The developer is bound by the same rules as on the API.
    let token = server
        .post("/orgs/acme/users/dev/tokens", json!({"id": "t1"}))
        .await;
    let mut dev = admin_machine(&server);
    dev.set("OPENPORCH_TOKEN", token["token"].as_str().unwrap());
    for args in [
        &["users", "create-service", "x"][..],
        &["env-types", "users", "development", "grant", "dev"][..],
        &["apps", "users", "shop", "grant", "dev", "owner"][..],
        &["audit-logs"][..],
        &["expire-audit-logs"][..],
    ] {
        let r = dev.porch(args, "").await;
        assert_eq!(r.code, 1, "{args:?}: {r:?}");
        assert!(r.err.contains("AUTH-002"), "{args:?}: {r:?}");
    }

    for args in [
        &["users", "set-role", "dev", "manager"][..],
        &["apps", "users", "shop", "revoke", "dev"][..],
        &["env-types", "users", "development", "revoke", "dev"][..],
    ] {
        let r = m.porch(args, "").await;
        assert_eq!(r.code, 0, "{args:?}: {r:?}");
    }
    let r = m.porch(&["users", "list", "-o", "yaml"], "").await;
    let list: Value = serde_yaml_ng::from_str(&r.out).unwrap();
    let dev_user = list
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "dev")
        .unwrap();
    assert_eq!(dev_user["role"], "manager");

    let r = m
        .porch(
            &[
                "users",
                "invite",
                "--role",
                "member",
                "--email",
                "pat@example.com",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("shown once"), "{r:?}");
    let r = m.porch(&["users", "invite", "--role", "member"], "").await;
    assert_eq!(r.code, 2, "{r:?}");

    // The built-in secret store and its grants.
    let r = m.porch(&["secretstores", "list"], "").await;
    assert!(r.out.lines().any(|l| l.starts_with("openporch")), "{r:?}");
    let r = m.porch(&["secretstores", "grants", "openporch"], "").await;
    assert_eq!(r.code, 0, "{r:?}");

    // The audit log shows what happened, filtered like the API.
    let r = m
        .porch(
            &[
                "audit-logs",
                "--path-prefix",
                "/orgs/acme/apps/shop/users",
                "--method",
                "delete",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let rows: Vec<&str> = r.out.lines().skip(1).collect();
    assert_eq!(rows.len(), 1, "{r:?}");
    assert!(rows[0].contains("DELETE") && rows[0].contains("/orgs/acme/apps/shop/users/dev"));
    let r = m.porch(&["audit-logs", "--limit", "2"], "").await;
    assert_eq!(r.out.lines().count(), 3, "{r:?}");
    assert!(r.err.contains("--before"), "{r:?}");
    assert_no_secret(&r, &server.admin);
    // `--all` follows the pages, for export.
    let r = m
        .porch(&["audit-logs", "--all", "--limit", "2", "-o", "json"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let all: Value = serde_json::from_str(&r.out).unwrap();
    assert!(all["entries"].as_array().unwrap().len() > 2, "{r:?}");
    assert!(all.get("next").is_none(), "{r:?}");
    // Nothing here is older than the retention period.
    let r = m.porch(&["expire-audit-logs", "--dry-run"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out
            .starts_with("0 entries older than 30 days would expire"),
        "{r:?}"
    );
    let r = m.porch(&["expire-audit-logs", "-o", "json"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let expired: Value = serde_json::from_str(&r.out).unwrap();
    assert_eq!(expired["expired"], 0, "{r:?}");

    // An ID of `..` cannot redirect a request to another route.
    let r = m.porch(&["apps", "users", "..", "revoke", "dev"], "").await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("`..`"), "{r:?}");
    let r = m.porch(&["users", "list"], "").await;
    assert!(r.out.lines().any(|l| l.starts_with("dev ")), "{r:?}");

    let r = m.porch(&["users", "remove", "dev"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let r = dev.porch(&["get", "apps"], "").await;
    assert!(r.err.contains("AUTH-001"), "{r:?}");
}
