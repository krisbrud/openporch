//! HTTP tests for `resource-catalog`, `resource-definitions`, the driver registry and
//! the definition scenarios of `secret-stores-and-references`.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

impl Server {
    pub(super) async fn put(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::PUT, path, Some(token), Some(body)).await
    }

    async fn created(&self, path: &str, body: Value) -> Value {
        let (s, b) = self.post(path, &self.admin, body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b
    }

    /// Records an active resource provisioned by `def` in `app/env`.
    async fn active_resource(&self, app: &str, env: &str, descriptor: &str, def: &str) {
        sqlx::query(
            "INSERT INTO active_resources (org_id, app_id, env_id, descriptor, guresid, \
             definition_id, definition_version, status, created_at, updated_at) \
             VALUES ('acme', ?, ?, ?, ?, ?, 1, 'provisioned', 0, 0)",
        )
        .bind(app)
        .bind(env)
        .bind(descriptor)
        .bind(format!("{app}-{env}-{def}"))
        .bind(def)
        .execute(self.state.store.pool())
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn built_in_types_are_listed_with_schemas() {
    let s = server().await;
    let member = s.member("mia", "member").await;
    let (status, types) = s.get("/orgs/acme/resources/types", &member).await;
    assert_eq!(status, StatusCode::OK, "{types}");
    let ids: Vec<&str> = types
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    for id in [
        "workload",
        "k8s-cluster",
        "k8s-namespace",
        "base-env",
        "config",
        "dns",
        "route",
        "postgres",
        "postgres-instance",
        "mysql",
        "redis",
        "s3",
        "sqs",
        "sns-topic",
        "dynamodb-table",
        "aws-role",
        "aws-policy",
        "k8s-service-account",
    ] {
        assert!(ids.contains(&id), "{id} missing");
    }
    for t in types.as_array().unwrap() {
        for field in ["use", "category", "inputs_schema", "outputs_schema"] {
            assert!(!t[field].is_null(), "{} has no {field}", t["id"]);
        }
    }

    let (_, workload) = s.get("/orgs/acme/resources/types/workload", &member).await;
    let inputs = &workload["inputs_schema"]["properties"];
    for key in ["id", "class", "spec", "dependencies"] {
        assert!(inputs.get(key).is_some(), "workload input {key}");
    }
    assert_ne!(
        workload["outputs_schema"]["properties"]["values"]["additionalProperties"],
        false
    );

    let (_, pg) = s.get("/orgs/acme/resources/types/postgres", &member).await;
    let outputs = &pg["outputs_schema"]["properties"];
    let names = |section: &str| {
        let mut v: Vec<String> = outputs[section]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    };
    assert_eq!(names("values"), ["host", "name", "port"]);
    assert_eq!(names("secrets"), ["password", "username"]);

    let (status, b) = s.get("/orgs/acme/resources/types/nope", &member).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn custom_types_are_org_prefixed_and_built_ins_are_fixed() {
    let s = server().await;
    let flags = json!({
        "id": "acme/feature-flags",
        "use": "direct",
        "outputs_schema": {"properties": {"secrets": {
            "type": "object",
            "properties": {"sdk_key": {"type": "string"}},
            "required": ["sdk_key"],
        }}},
    });

    // Only administrators change the catalog.
    for role in ["manager", "member"] {
        let token = s.member(&format!("u-{role}"), role).await;
        let (status, b) = s
            .post("/orgs/acme/resources/types", &token, flags.clone())
            .await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    }

    let created = s.created("/orgs/acme/resources/types", flags.clone()).await;
    assert_eq!(created["id"], "acme/feature-flags");
    let path = "/orgs/acme/resources/types/acme%2Ffeature-flags";
    let (status, b) = s.get(path, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(
        b["outputs_schema"]["properties"]["secrets"]["required"],
        json!(["sdk_key"])
    );
    let (status, b) = s.post("/orgs/acme/resources/types", &s.admin, flags).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // Definitions may target the custom type.
    s.created(
        "/orgs/acme/resources/defs",
        json!({
            "id": "flags", "type": "acme/feature-flags", "driver_type": "openporch/echo",
            "driver_inputs": {"secrets": {"sdk_key": "k-123"}},
            "criteria": [{}],
        }),
    )
    .await;
    let (status, b) = s.delete(path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(b["message"].as_str().unwrap().contains("flags"), "{b}");

    // The custom type can be updated; its ID cannot change.
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"use": "direct", "name": "Feature flags", "category": "config"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["name"], "Feature flags");
    let (status, _) = s
        .put(path, &s.admin, json!({"id": "acme/other", "use": "direct"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Custom IDs carry this organization's prefix.
    for id in ["other/flags", "flags"] {
        let (status, b) = s
            .post(
                "/orgs/acme/resources/types",
                &s.admin,
                json!({"id": id, "use": "direct"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {b}");
    }

    // Built-in types are neither modifiable nor deletable.
    let (status, b) = s
        .put(
            "/orgs/acme/resources/types/postgres",
            &s.admin,
            json!({"use": "direct"}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .delete("/orgs/acme/resources/types/postgres", &s.admin)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .post(
            "/orgs/acme/resources/types",
            &s.admin,
            json!({"id": "postgres", "use": "direct"}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // Co-provisioning keeps the type too.
    s.delete("/orgs/acme/resources/defs/flags", &s.admin).await;
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "with-flags", "type": "redis", "driver_type": "openporch/echo",
               "provision": {"acme/feature-flags": {}}}),
    )
    .await;
    let (status, b) = s.delete(path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(b["message"].as_str().unwrap().contains("with-flags"), "{b}");

    // Once unused, the custom type can be deleted.
    s.delete("/orgs/acme/resources/defs/with-flags", &s.admin)
        .await;
    let (status, b) = s.delete(path, &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(s.get(path, &s.admin).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn classes_exist_per_type_and_are_kept_while_referenced() {
    let s = server().await;
    let classes = "/orgs/acme/resources/types/s3/classes";
    let (status, b) = s.get(classes, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b[0]["id"], "default");

    let dev = s.member("dev", "member").await;
    let (status, b) = s.post(classes, &dev, json!({"id": "sensitive"})).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    let (status, b) = s.post(classes, &s.admin, json!({"id": "default"})).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .post(
            "/orgs/acme/resources/types/nope/classes",
            &s.admin,
            json!({"id": "x1"}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    // Criteria can only use classes that exist for the definition's type.
    let def = json!({
        "id": "sensitive-s3", "type": "s3", "driver_type": "openporch/echo",
        "criteria": [{"class": "sensitive"}],
    });
    let (status, b) = s
        .post("/orgs/acme/resources/defs", &s.admin, def.clone())
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b["message"].as_str().unwrap().contains("sensitive"), "{b}");

    s.created(
        classes,
        json!({"id": "sensitive", "description": "Encrypted with a CMK"}),
    )
    .await;
    let (_, b) = s.get(classes, &dev).await;
    assert_eq!(b[1]["id"], "sensitive");
    assert_eq!(b[1]["description"], "Encrypted with a CMK");
    let (_, all) = s.get("/orgs/acme/resources/classes", &dev).await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    s.created("/orgs/acme/resources/defs", def).await;

    let class = format!("{classes}/sensitive");
    let (status, b) = s.delete(&class, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("sensitive-s3"),
        "{b}"
    );
    let (status, b) = s.delete(&format!("{classes}/default"), &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    s.delete("/orgs/acme/resources/defs/sensitive-s3", &s.admin)
        .await;
    let (status, b) = s.delete(&class, &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
}

#[tokio::test]
async fn definition_over_the_api() {
    let s = server().await;
    let def = json!({
        "id": "stg-redis", "type": "redis", "driver_type": "openporch/opentofu",
        "driver_inputs": {"values": {"source": {"url": "https://example.com/m.git"}}},
        "criteria": [{"env_type": "staging"}],
    });
    for role in ["manager", "member"] {
        let token = s.member(&format!("u-{role}"), role).await;
        let (status, b) = s
            .post("/orgs/acme/resources/defs", &token, def.clone())
            .await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    }
    // An application developer is no administrator either.
    s.app("shop").await;
    let developer = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let (status, b) = s
        .post("/orgs/acme/resources/defs", &developer, def.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let created = s.created("/orgs/acme/resources/defs", def.clone()).await;
    assert_eq!(created["version"], 1);
    let member = s.member("mia", "member").await;
    let (status, b) = s.get("/orgs/acme/resources/defs/stg-redis", &member).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["type"], "redis");
    assert_eq!(b["driver_type"], "openporch/opentofu");
    assert_eq!(b["criteria"][0]["env_type"], "staging");
    assert!(b["criteria"][0]["id"].is_string(), "{b}");
    let (_, list) = s.get("/orgs/acme/resources/defs", &member).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, b) = s.post("/orgs/acme/resources/defs", &s.admin, def).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // Unknown drivers, types and accounts are named.
    for (field, value, named) in [
        ("driver_type", "acme/nope", "acme/nope"),
        ("type", "acme/nope", "acme/nope"),
        ("driver_account", "aws-nope", "aws-nope"),
        ("driver_account", "aws-${broken", "driver_account"),
        ("driver_account", "${unknown.path}", "driver_account"),
    ] {
        let mut body = json!({
            "id": "bad", "type": "redis", "driver_type": "openporch/echo", "criteria": [{}],
        });
        body[field] = json!(value);
        let (status, b) = s.post("/orgs/acme/resources/defs", &s.admin, body).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(b["message"].as_str().unwrap().contains(named), "{b}");
    }
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "bad", "type": "redis", "driver_type": "openporch/echo",
                   "provision": {"aws-nope": {}}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b["message"].as_str().unwrap().contains("aws-nope"), "{b}");
    // A placeholder account is resolved at deployment time.
    s.created(
        "/orgs/acme/resources/defs",
        json!({
            "id": "acct", "type": "redis", "driver_type": "openporch/echo",
            "driver_account": "${resources.config#aws.outputs.account}",
        }),
    )
    .await;
    assert_eq!(
        s.get("/orgs/acme/resources/defs/bad", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );

    // Updates replace everything but the type and criteria and bump the version.
    let (status, b) = s
        .put(
            "/orgs/acme/resources/defs/stg-redis",
            &s.admin,
            json!({"driver_type": "openporch/echo", "driver_inputs": {"values": {"host": "h"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["version"], 2);
    assert_eq!(b["driver_type"], "openporch/echo");
    assert_eq!(b["driver_inputs"]["values"], json!({"host": "h"}));
    assert_eq!(b["criteria"].as_array().unwrap().len(), 1);
    let (status, _) = s
        .put(
            "/orgs/acme/resources/defs/stg-redis",
            &s.admin,
            json!({"type": "postgres", "driver_type": "openporch/echo"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn definition_secrets_are_stored_as_references() {
    let s = server().await;
    s.grant_secret("openporch", "shared/key").await;
    let created = s
        .created(
            "/orgs/acme/resources/defs",
            json!({
                "id": "pg", "type": "postgres-instance", "driver_type": "openporch/echo",
                "driver_inputs": {
                    "values": {"host": "db.internal"},
                    "secrets": {
                        "password": "hunter2",
                        "nested": {"token": "s3cr3t-token"},
                        "templated": "${resources.config.outputs.password}",
                    },
                    "secret_refs": {"variables": {
                        "username": {"value": "admin-user"},
                        "pinned": {"store": "openporch", "ref": "shared/key", "version": "3"},
                    }},
                },
                "criteria": [{}],
            }),
        )
        .await;
    let (_, read) = s.get("/orgs/acme/resources/defs/pg", &s.admin).await;
    // Org viewers read the same references and no values.
    let viewer = s.member("auditor", "orgViewer").await;
    let (status, viewed) = s.get("/orgs/acme/resources/defs/pg", &viewer).await;
    assert_eq!(status, StatusCode::OK, "{viewed}");
    assert_eq!(viewed, read);
    for body in [&created, &read, &viewed] {
        let text = body.to_string();
        for secret in ["hunter2", "s3cr3t-token", "admin-user"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let refs = &body["driver_inputs"]["secret_refs"];
        assert_eq!(
            refs["password"],
            json!({"store": "openporch", "ref": "defs/pg/password", "version": "1"})
        );
        assert_eq!(refs["nested"]["token"]["ref"], "defs/pg/nested/token");
        assert_eq!(
            refs["variables"]["username"]["ref"],
            "defs/pg/variables/username"
        );
        assert_eq!(
            refs["variables"]["pinned"],
            json!({"store": "openporch", "ref": "shared/key", "version": "3"})
        );
        // Placeholder templates stay inline; literals do not.
        assert_eq!(
            body["driver_inputs"]["secrets"],
            json!({"templated": "${resources.config.outputs.password}"})
        );
    }

    // The value really is in the built-in store.
    use openporch_store::secrets::SecretStore as _;
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let r =
        serde_json::from_value(read["driver_inputs"]["secret_refs"]["password"].clone()).unwrap();
    assert_eq!(primary.read(&r).await.unwrap().expose(), "hunter2");

    // Writing the same key again creates a new version that the reference pins.
    let (status, b) = s
        .put(
            "/orgs/acme/resources/defs/pg",
            &s.admin,
            json!({
                "driver_type": "openporch/echo",
                "driver_inputs": {"secret_refs": {"password": {"value": "correct-horse"}}},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains("correct-horse"));
    assert_eq!(
        b["driver_inputs"]["secret_refs"]["password"]["version"],
        "2"
    );

    // Invalid secret inputs are rejected without echoing values.
    for inputs in [
        json!({"secrets": {"password": "hunter2"}, "secret_refs": {"password": {"store": "openporch", "ref": "x"}}}),
        json!({"secret_refs": {"password": {"store": "openporch", "ref": "${context.env.id}"}}}),
        json!({"secret_refs": {"password": "hunter2"}}),
        json!({"secret_refs": {"password": {"store": "prod-sm", "ref": "shop/db"}}}),
        json!({"secrets": {"port": 5432}}),
    ] {
        let (status, b) = s
            .post(
                "/orgs/acme/resources/defs",
                &s.admin,
                json!({"id": "bad", "type": "postgres", "driver_type": "openporch/echo", "driver_inputs": inputs}),
            )
            .await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(!b.to_string().contains("hunter2"), "{b}");
    }
    // A request rejected for any reason writes no secret.
    for invalid in [
        json!({"driver_account": "aws-nope"}),
        json!({"criteria": [{"class": "nope"}]}),
        json!({"driver_type": "acme/nope"}),
    ] {
        let mut body = json!({"id": "bad", "type": "postgres", "driver_type": "openporch/echo",
                              "driver_inputs": {"secrets": {"password": "hunter2"}}});
        for (k, v) in invalid.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, b) = s.post("/orgs/acme/resources/defs", &s.admin, body).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    }
    let written: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM secret_versions WHERE ref LIKE 'defs/bad/%'")
            .fetch_one(s.state.store.pool())
            .await
            .unwrap();
    assert_eq!(written, 0);

    // A literal that merely contains `${` is a secret, not a template.
    let b = s
        .created(
            "/orgs/acme/resources/defs",
            json!({"id": "odd", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secrets": {"password": "abc${def"}}}),
        )
        .await;
    assert!(!b.to_string().contains("abc"), "{b}");
    assert_eq!(
        b["driver_inputs"]["secret_refs"]["password"]["ref"],
        "defs/odd/password"
    );

    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "bad", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"p": {"store": "openporch", "ref": "${context.env.id}"}}}}),
        )
        .await;
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("placeholders are not allowed"),
        "{status} {b}"
    );
}

#[tokio::test]
async fn criteria_are_managed_one_by_one() {
    let s = server().await;
    s.created(
        "/orgs/acme/resources/types/postgres/classes",
        json!({"id": "large"}),
    )
    .await;
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "prod-postgres", "type": "postgres", "driver_type": "openporch/echo"}),
    )
    .await;
    let criteria = "/orgs/acme/resources/defs/prod-postgres/criteria";
    let viewer = s.member("viewer", "member").await;
    let (status, b) = s
        .post(criteria, &viewer, json!({"env_type": "production"}))
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    let c = s
        .created(
            criteria,
            json!({"env_type": "production", "class": "large"}),
        )
        .await;
    let id = c["id"].as_str().unwrap().to_owned();
    let (_, def) = s
        .get("/orgs/acme/resources/defs/prod-postgres", &s.admin)
        .await;
    assert_eq!(
        def["criteria"],
        json!([{"id": id, "env_type": "production", "class": "large"}])
    );
    let (status, b) = s
        .post(
            criteria,
            &s.admin,
            json!({"env_type": "production", "class": "large"}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .post(criteria, &s.admin, json!({"environment": "production"}))
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    let (status, _) = s.delete(&format!("{criteria}/{id}"), &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, def) = s
        .get("/orgs/acme/resources/defs/prod-postgres", &s.admin)
        .await;
    assert_eq!(def["criteria"], json!([]));
    let (status, b) = s.delete(&format!("{criteria}/{id}"), &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs/nope/criteria",
            &s.admin,
            json!({"env_type": "production"}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn definition_in_use_is_kept() {
    let s = server().await;
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "prod-postgres", "type": "postgres", "driver_type": "openporch/echo",
               "criteria": [{"env_type": "production"}]}),
    )
    .await;
    s.active_resource(
        "shop",
        "production",
        "postgres.default.modules.web.externals.db",
        "prod-postgres",
    )
    .await;
    let manager = s.member("carol", "manager").await;
    let path = "/orgs/acme/resources/defs/prod-postgres";
    let (status, b) = s.delete(path, &manager).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s.delete(path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("shop/production"),
        "{b}"
    );
    assert_eq!(s.get(path, &s.admin).await.0, StatusCode::OK);
}

#[tokio::test]
async fn external_drivers_are_registered() {
    let s = server().await;
    let member = s.member("mia", "member").await;
    let (status, list) = s.get("/orgs/acme/resources/drivers", &member).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    for id in [
        "openporch/echo",
        "openporch/none",
        "openporch/template",
        "openporch/opentofu",
    ] {
        assert!(ids.contains(&id), "{id}");
    }

    let vault = json!({
        "id": "acme/vault-secret",
        "target": "https://driver.internal/vault",
        "token": {"value": "bearer-s3cret"},
    });
    let (status, b) = s
        .post("/orgs/acme/resources/drivers", &member, vault.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let created = s
        .created("/orgs/acme/resources/drivers", vault.clone())
        .await;
    assert!(!created.to_string().contains("bearer-s3cret"));
    assert_eq!(created["token"]["ref"], "drivers/acme/vault-secret/token");
    assert_eq!(created["target"], "https://driver.internal/vault");
    let path = "/orgs/acme/resources/drivers/acme%2Fvault-secret";
    let (status, b) = s.get(path, &member).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains("bearer-s3cret"));
    // The registration pins the protocol, the cookie version and the
    // capabilities, with defaults when left out.
    assert_eq!(b["protocol_version"], "openporch-http/1", "{b}");
    assert_eq!(b["cookie_version"], 1, "{b}");
    assert_eq!(
        b["capabilities"],
        json!({"targets": {"resources": true, "workloads": false, "triggers": false},
               "observe": false, "recover": false, "preview": false, "cancel": false})
    );
    let pinned = s
        .created(
            "/orgs/acme/resources/drivers",
            json!({"id": "acme/pinned", "target": "https://driver.internal/pinned",
                   "protocol_version": "openporch-http/1", "cookie_version": 3,
                   "outputs_schema": {"type": "object"},
                   "capabilities": {"targets": {"resources": false, "workloads": true},
                                    "observe": true, "recover": true, "cancel": true}}),
        )
        .await;
    let (status, b) = s
        .get("/orgs/acme/resources/drivers/acme%2Fpinned", &member)
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b, pinned);
    assert_eq!(b["cookie_version"], 3, "{b}");
    assert_eq!(b["outputs_schema"], json!({"type": "object"}), "{b}");
    assert_eq!(
        b["capabilities"],
        json!({"targets": {"resources": false, "workloads": true, "triggers": false},
               "observe": true, "recover": true, "preview": false, "cancel": true})
    );

    // A definition may use a driver only for what it declares it provisions,
    // when it is created and when it is changed.
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "web", "type": "workload", "driver_type": "acme/vault-secret"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("driver `acme/vault-secret` cannot provision resources of type `workload`"),
        "{b}"
    );
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "pinned-pg", "type": "postgres", "driver_type": "acme/pinned"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"].as_str().unwrap().contains("`resources`"),
        "{b}"
    );
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "web", "type": "workload", "driver_type": "acme/pinned"}),
    )
    .await;
    let (status, b) = s
        .put(
            "/orgs/acme/resources/defs/web",
            &s.admin,
            json!({"driver_type": "acme/vault-secret"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"].as_str().unwrap().contains("`workloads`"),
        "{b}"
    );
    let (status, b) = s.delete("/orgs/acme/resources/defs/web", &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");

    // A registration under the same ID cannot inherit its predecessor's token.
    let (status, b) = s.delete(path, &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
    let (status, b) = s
        .post(
            "/orgs/acme/resources/drivers",
            &s.admin,
            json!({"id": "acme/vault-secret", "target": "https://elsewhere.internal/vault",
                   "token": {"store": "openporch", "ref": "drivers/acme/vault-secret/token"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"].as_str().unwrap().contains("own secrets"),
        "{b}"
    );
    s.created("/orgs/acme/resources/drivers", vault.clone())
        .await;

    // Nor can a registration be deleted while an operation it ran has an
    // outcome nobody knows yet: recovery observes with that driver.
    sqlx::query(
        "INSERT INTO operations (id, org_id, app_id, env_id, deployment_id, guresid, kind, \
         input_fingerprint, state, started_at, driver_type) \
         VALUES ('op-open', 'acme', 'shop', 'development', 'd-1', 'g-1', 'put', 'f', \
         'unknown', 0, 'acme/vault-secret')",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let (status, b) = s.delete(path, &s.admin).await;
    assert_eq!(status, StatusCode::CONFLICT, "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("not known yet"),
        "{b}"
    );
    sqlx::query("UPDATE operations SET state = 'failed' WHERE id = 'op-open'")
        .execute(s.state.store.pool())
        .await
        .unwrap();

    for (body, message) in [
        (json!({"capabilities": {"targets": {"pods": true}}}), "pods"),
        (
            json!({"protocol_version": "openporch-http/2"}),
            "`openporch-http/2` is not supported",
        ),
        // The protocol has no preview call.
        (
            json!({"capabilities": {"preview": true}}),
            "`capabilities.preview` must be false",
        ),
        (json!({"capabilities": {"teleport": true}}), "teleport"),
        (json!({"outputs_schema": []}), "`outputs_schema`"),
    ] {
        let mut body = body;
        body["id"] = json!("acme/refused");
        body["target"] = json!("https://driver.internal/refused");
        let (status, b) = s.post("/orgs/acme/resources/drivers", &s.admin, body).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(b["message"].as_str().unwrap().contains(message), "{b}");
    }
    let (status, b) = s
        .post("/orgs/acme/resources/drivers", &s.admin, vault)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    let (status, b) = s
        .post(
            "/orgs/acme/resources/drivers",
            &s.admin,
            json!({"id": "acme/other", "target": "https://d.internal",
                   "token": {"store": "missing", "ref": "x"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b["message"].as_str().unwrap().contains("missing"), "{b}");

    for (id, target) in [
        ("openporch/mine", "https://driver.internal"),
        ("vault-secret", "https://driver.internal"),
        ("acme/plain", "http://driver.internal"),
        ("acme/creds", "https://user:pw@driver.internal"),
        // The GUResID is appended to the path, so neither may be set.
        ("acme/query", "https://driver.internal/x?k=v"),
        ("acme/fragment", "https://driver.internal/x#f"),
    ] {
        let (status, b) = s
            .post(
                "/orgs/acme/resources/drivers",
                &s.admin,
                json!({"id": id, "target": target}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {b}");
        assert!(!b.to_string().contains("pw@"), "{b}");
    }

    // Definitions may use the registered driver, which then stays.
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "vault", "type": "config", "driver_type": "acme/vault-secret"}),
    )
    .await;
    let (status, b) = s.delete(path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(b["message"].as_str().unwrap().contains("vault"), "{b}");
    let (status, b) = s
        .delete("/orgs/acme/resources/drivers/openporch%2Fecho", &s.admin)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // An active resource keeps its driver after the definition moves away.
    s.active_resource("shop", "production", "config.default.shared.vault", "vault")
        .await;
    sqlx::query("UPDATE active_resources SET driver_type = 'acme/vault-secret'")
        .execute(s.state.store.pool())
        .await
        .unwrap();
    s.put(
        "/orgs/acme/resources/defs/vault",
        &s.admin,
        json!({"driver_type": "openporch/echo"}),
    )
    .await;
    let (status, b) = s.delete(path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("shop/production"),
        "{b}"
    );
    sqlx::query("DELETE FROM active_resources")
        .execute(s.state.store.pool())
        .await
        .unwrap();
    let (status, b) = s.delete(path, &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{b}");
}

#[tokio::test]
async fn external_drivers_are_replaced() {
    use openporch_store::secrets::SecretStore as _;
    let s = server().await;
    let member = s.member("mia", "member").await;
    let path = "/orgs/acme/resources/drivers/acme%2Fvault-db";
    let created = s
        .created(
            "/orgs/acme/resources/drivers",
            json!({"id": "acme/vault-db", "target": "https://drivers.acme.example/vault-db",
                   "token": {"value": "first-s3cret"}, "capabilities": {"observe": true}}),
        )
        .await;
    let first = created["token"].clone();
    assert_eq!(first["ref"], "drivers/acme/vault-db/token", "{created}");

    let (status, b) = s
        .put(
            path,
            &member,
            json!({"target": "https://drivers.acme.example/vault-db"}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .put(
            "/orgs/acme/resources/drivers/acme%2Fmissing",
            &s.admin,
            json!({"target": "https://drivers.acme.example/missing"}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (status, b) = s
        .put(
            "/orgs/acme/resources/drivers/openporch%2Fecho",
            &s.admin,
            json!({"target": "https://drivers.acme.example/echo"}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    // Whatever the body.
    let (status, b) = s
        .call(
            Method::PUT,
            "/orgs/acme/resources/drivers/openporch%2Fecho",
            Some(&s.admin),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .put(
            "/orgs/acme/resources/drivers/openporch%2Fecho",
            &s.admin,
            json!({"teleport": true}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"id": "acme/other", "target": "https://drivers.acme.example/vault-db"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    // A rotated token is a new version; the target moves in place, normalized.
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://Drivers.ACME.example:443/v2", "token": {"value": "second-s3cret"},
                   "cookie_version": 2, "capabilities": {"observe": true, "recover": true}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains("s3cret"), "{b}");
    assert_eq!(b["target"], "https://drivers.acme.example/v2", "{b}");
    assert_eq!(b["cookie_version"], 2, "{b}");
    assert_eq!(b["capabilities"]["recover"], true, "{b}");
    let second = b["token"].clone();
    assert_eq!(second["ref"], first["ref"], "{b}");
    assert_ne!(second["version"], first["version"], "{b}");
    let (_, got) = s.get(path, &member).await;
    assert_eq!(got, b);
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let token: openporch_core::model::SecretRef = serde_json::from_value(second.clone()).unwrap();
    assert_eq!(
        primary.read(&token).await.unwrap().expose(),
        "second-s3cret"
    );

    // The token it holds is kept when given back; an earlier version, or
    // another secret under the driver, must be entered again.
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v3", "token": second}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["token"], second, "{b}");
    // Pins and capabilities left out stay as they are.
    assert_eq!(b["cookie_version"], 2, "{b}");
    assert_eq!(b["protocol_version"], "openporch-http/1", "{b}");
    assert_eq!(b["capabilities"]["recover"], true, "{b}");
    // A given capabilities object replaces them, with its own defaults.
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v3", "token": second,
                   "capabilities": {"observe": true}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["capabilities"]["recover"], false, "{b}");
    assert_eq!(b["cookie_version"], 2, "{b}");
    for token in [
        first.clone(),
        json!({"store": "openporch", "ref": "drivers/acme/vault-db/token"}),
        json!({"store": "openporch", "ref": "drivers/acme/vault-db/other", "version": "1"}),
    ] {
        let (status, b) = s
            .put(
                path,
                &s.admin,
                json!({"target": "https://drivers.acme.example/v3", "token": token}),
            )
            .await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(
            b["message"].as_str().unwrap().contains("own secrets"),
            "{b}"
        );
    }
    // Validated like a registration.
    for (body, message) in [
        (json!({"target": "http://drivers.acme.example"}), "https"),
        (
            json!({"target": "https://drivers.acme.example", "capabilities": {"preview": true}}),
            "`capabilities.preview` must be false",
        ),
        (
            json!({"target": "https://drivers.acme.example", "protocol_version": "openporch-http/2"}),
            "not supported",
        ),
        (
            json!({"target": "https://drivers.acme.example", "token": {"store": "missing", "ref": "x"}}),
            "missing",
        ),
    ] {
        let (status, b) = s.put(path, &s.admin, body).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(b["message"].as_str().unwrap().contains(message), "{b}");
    }

    // A definition keeps the types it uses covered.
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "vault", "type": "config", "driver_type": "acme/vault-db"}),
    )
    .await;
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v3", "token": second,
                   "capabilities": {"targets": {"resources": false, "workloads": true}}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(b["message"].as_str().unwrap().contains("`vault`"), "{b}");

    // Nor is it replaced while an operation it ran has an unknown outcome.
    sqlx::query(
        "INSERT INTO operations (id, org_id, app_id, env_id, deployment_id, guresid, kind, \
         input_fingerprint, state, started_at, driver_type) \
         VALUES ('op-open', 'acme', 'shop', 'development', 'd-1', 'g-1', 'put', 'f', \
         'started', 0, 'acme/vault-db')",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v4", "token": {"value": "third-s3cret"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("not known yet"),
        "{b}"
    );
    assert!(!b.to_string().contains("s3cret"), "{b}");
    let (_, got) = s.get(path, &member).await;
    assert_eq!(got["token"], second, "{got}");
    assert_eq!(got["target"], "https://drivers.acme.example/v3", "{got}");
    sqlx::query("UPDATE operations SET state = 'succeeded' WHERE id = 'op-open'")
        .execute(s.state.store.pool())
        .await
        .unwrap();

    // Leaving the token out removes it.
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v4"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(b.get("token").is_none(), "{b}");

    // Active resources hold cookies of the version the driver writes, so it
    // stays while they exist.
    s.active_resource("shop", "production", "config.default.shared.vault", "vault")
        .await;
    sqlx::query("UPDATE active_resources SET driver_type = 'acme/vault-db'")
        .execute(s.state.store.pool())
        .await
        .unwrap();
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v4", "cookie_version": 3}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let message = b["message"].as_str().unwrap();
    assert!(
        message.contains("`shop-production-vault` (shop/production)"),
        "{b}"
    );
    // The first ten GUResIDs are named, with a count of the rest.
    for n in 0..11 {
        sqlx::query(
            "INSERT INTO active_resources (org_id, app_id, env_id, descriptor, guresid, \
             definition_id, definition_version, status, created_at, updated_at, driver_type) \
             VALUES ('acme', 'shop', 'development', ?, ?, 'vault', 1, 'provisioned', 0, 0, \
             'acme/vault-db')",
        )
        .bind(format!("config.default.shared.v{n:02}"))
        .bind(format!("g-{n:02}"))
        .execute(s.state.store.pool())
        .await
        .unwrap();
    }
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v4", "cookie_version": 3}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let message = b["message"].as_str().unwrap();
    assert!(
        message.contains("`g-09` (shop/development) and 2 more"),
        "{b}"
    );
    assert!(!message.contains("g-10"), "{b}");
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v5", "cookie_version": 2}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");

    let (_, page) = s
        .get(
            "/orgs/acme/audit-logs?method=PUT&path_prefix=/orgs/acme/resources/drivers",
            &s.admin,
        )
        .await;
    let actions: Vec<&str> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["result"] != "denied")
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"driver.update"), "{page}");
    assert!(!page.to_string().contains("s3cret"), "{page}");
}

/// The Actor recorded scenario: a definition names the service user that
/// created it.
#[tokio::test]
async fn a_definition_records_the_service_user_that_created_it() {
    let s = server().await;
    let ci = s.member("ci", "administrator").await;
    let (status, created) = s
        .post(
            "/orgs/acme/resources/defs",
            &ci,
            json!({"id": "dns", "type": "dns", "driver_type": "openporch/echo"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["created_by"], "ci", "{created}");
    let (_, read) = s.get("/orgs/acme/resources/defs/dns", &s.admin).await;
    assert_eq!(read["created_by"], "ci", "{read}");
    let (_, list) = s.get("/orgs/acme/resources/defs", &s.admin).await;
    let listed = list
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "dns")
        .unwrap();
    assert_eq!(listed["created_by"], "ci", "{listed}");
}

/// The Incompatible cookie upgrade scenario: a driver whose cookie version
/// changes cannot read the cookies its active resources hold, so the change
/// is refused naming their GUResIDs, and the registration and the resources'
/// management metadata stay as they were.
#[tokio::test]
async fn a_cookie_version_change_is_refused_while_resources_hold_cookies() {
    let s = server().await;
    let path = "/orgs/acme/resources/drivers/acme%2Fvault-db";
    s.created(
        "/orgs/acme/resources/drivers",
        json!({"id": "acme/vault-db", "target": "https://drivers.acme.example/vault-db",
               "token": {"value": "first-s3cret"}, "capabilities": {"observe": true}}),
    )
    .await;
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "vault", "type": "config", "driver_type": "acme/vault-db"}),
    )
    .await;
    let (_, registered) = s.get(path, &s.admin).await;
    assert_eq!(registered["cookie_version"], 1, "{registered}");
    s.active_resource("shop", "production", "config.default.shared.vault", "vault")
        .await;
    sqlx::query(
        "UPDATE active_resources SET driver_type = 'acme/vault-db', cookie_key_id = 'k1', \
         cookie_nonce = X'00', cookie = X'0102030405', recovery = '{\"note\": 1}'",
    )
    .execute(s.state.store.pool())
    .await
    .unwrap();
    let metadata = || async {
        sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                Option<Vec<u8>>,
                String,
                String,
            ),
        >(
            "SELECT guresid, driver_type, cookie_key_id, cookie, recovery, status \
             FROM active_resources",
        )
        .fetch_all(s.state.store.pool())
        .await
        .unwrap()
    };
    let before = metadata().await;

    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v2", "token": registered["token"],
                   "cookie_version": 2}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let message = b["message"].as_str().unwrap();
    assert!(
        message.contains("`shop-production-vault` (shop/production)"),
        "{b}"
    );
    assert!(!b.to_string().contains("s3cret"), "{b}");
    // Nothing changed: not the registration, not the resources.
    assert_eq!(s.get(path, &s.admin).await.1, registered);
    assert_eq!(metadata().await, before);

    // Without active resources holding its cookies, the version may change.
    sqlx::query("DELETE FROM active_resources")
        .execute(s.state.store.pool())
        .await
        .unwrap();
    let (status, b) = s
        .put(
            path,
            &s.admin,
            json!({"target": "https://drivers.acme.example/v2", "token": registered["token"],
                   "cookie_version": 2}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["cookie_version"], 2, "{b}");
}

#[tokio::test]
async fn catalog_changes_are_audited() {
    let s = server().await;
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "dns", "type": "dns", "driver_type": "openporch/echo"}),
    )
    .await;
    let (_, page) = s
        .get(
            "/orgs/acme/audit-logs?method=POST&path_prefix=/orgs/acme/resources/defs",
            &s.admin,
        )
        .await;
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{page}");
    assert_eq!(entries[0]["action"], "definition.create");
    assert_eq!(entries[0]["target"], "dns");
}

#[tokio::test]
async fn catalog_changes_are_rechecked_with_the_change() {
    use openporch_store::{AuditIntent, CatalogStore, StoreError};
    let s = server().await;
    s.member("carol", "manager").await;
    let def: openporch_core::model::ResourceDefinition = serde_json::from_value(json!({
        "id": "dns", "type": "dns", "driver_type": "openporch/echo",
    }))
    .unwrap();
    let audit = || {
        AuditIntent::accepted(
            "acme",
            "carol",
            "POST",
            "/orgs/acme/resources/defs",
            "definition.create",
        )
    };
    let e = s
        .state
        .store
        .create_definition("acme", &def, audit())
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::Forbidden(_)), "{e}");
    let e = s
        .state
        .store
        .delete_type("acme", "acme/x", audit())
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::Forbidden(_)), "{e}");
}

#[tokio::test]
async fn malformed_secret_bodies_are_not_echoed() {
    let s = server().await;
    let (status, b) = s
        .post(
            "/orgs/acme/resources/defs",
            &s.admin,
            json!({"id": "pg", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secrets": "hunter2"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(!b.to_string().contains("hunter2"), "{b}");
    let (status, b) = s
        .post(
            "/orgs/acme/resources/drivers",
            &s.admin,
            json!({"id": "acme/xy", "target": "https://d.example", "token": {"value": 7, "x": "hunter2"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(!b.to_string().contains("hunter2"), "{b}");
}

#[tokio::test]
async fn secrets_of_rejected_changes_are_discarded() {
    use openporch_store::secrets::{SecretStore as _, SecretValue};
    let s = server().await;
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let r = primary
        .write("defs/x/password", &SecretValue::new("hunter2"))
        .await
        .unwrap();
    crate::secret_inputs::discard(&primary, vec![r.clone()]).await;
    assert!(primary.read(&r).await.is_err());
}

#[tokio::test]
async fn opentofu_definition_inputs_are_checked_when_written() {
    // `Neither source nor script` in `opentofu-driver`, and the update path.
    let s = server().await;
    let def = |inputs: Value| {
        json!({"id": "tofu-queue", "type": "sqs", "driver_type": "openporch/opentofu",
               "driver_inputs": inputs, "criteria": [{}]})
    };
    for (inputs, want) in [
        (json!({"values": {}}), "set `source`, `script` or both"),
        (
            json!({"values": {"script": "", "files": {"override.tf": ""}}}),
            "would replace the script, the backend",
        ),
        (
            json!({"values": {"source": {"url": "https://user:pw@git.example/m"}}}),
            "must not contain credentials",
        ),
        (
            json!({"values": {"script": ""}, "secrets": {"password": "${resources.x.outputs.y}"}}),
            "unknown OpenTofu driver secret input `password`",
        ),
        (
            json!({"values": {"script": "", "timeout": "15 minutes"}}),
            "`timeout` must be",
        ),
        (
            json!({"values": {"script": "", "credentials_config": {"environment": {"AWS_ENDPOINT_URL": "Region"}}}}),
            "cannot be set by `credentials_config`",
        ),
        (
            json!({"values": {"script": ""}, "secret_refs": {"files": {"main.tf": {"value": "x"}}},
                   "secrets": {"files": {"ok.txt": "${resources.x.outputs.y}"}}}),
            "would replace the script",
        ),
        (
            json!({"values": {"script": "", "variables": {"resource": "x"}}}),
            "the variable `resource` is reserved",
        ),
        (
            json!({"values": {"script": ""}, "secrets": {"variables": {"resource_secrets": "x"}}}),
            "the variable `resource_secrets` is reserved",
        ),
    ] {
        let (status, b) = s
            .post("/orgs/acme/resources/defs", &s.admin, def(inputs))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
        assert!(b["message"].as_str().unwrap().contains(want), "{b}");
    }
    // A placeholder can stand for a whole input.
    s.created(
        "/orgs/acme/resources/defs",
        json!({"id": "tofu-vars", "type": "sqs", "driver_type": "openporch/opentofu",
               "driver_inputs": {"values": {"script": "", "variables": "${resources.cfg.outputs.vars}"}},
               "criteria": [{}]}),
    )
    .await;
    // Placeholders in `source` are left to deployment.
    s.created(
        "/orgs/acme/resources/defs",
        def(
            json!({"values": {"source": {"url": "${resources.repo.outputs.url}"}},
                   "secret_refs": {"source": {"password": {"value": "token-1"}}}}),
        ),
    )
    .await;
    let (status, b) = s
        .put(
            "/orgs/acme/resources/defs/tofu-queue",
            &s.admin,
            json!({"driver_type": "openporch/opentofu", "driver_inputs": {"values": {}}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
}
