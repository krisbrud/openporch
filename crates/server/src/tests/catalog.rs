//! HTTP tests for `resource-catalog`, `resource-definitions`, the driver registry and
//! the definition scenarios of `secret-stores-and-references`.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

impl Server {
    async fn put(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
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

    // Once unused, the custom type can be deleted.
    s.delete("/orgs/acme/resources/defs/flags", &s.admin).await;
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
    for body in [&created, &read] {
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
    let (status, b) = s
        .post("/orgs/acme/resources/drivers", &s.admin, vault)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    for (id, target) in [
        ("openporch/mine", "https://driver.internal"),
        ("vault-secret", "https://driver.internal"),
        ("acme/plain", "http://driver.internal"),
        ("acme/creds", "https://user:pw@driver.internal"),
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
