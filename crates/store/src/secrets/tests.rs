use std::sync::Arc;

use super::*;
use crate::{Ownership, SqliteStore, Store};

struct Fixture {
    _dir: tempfile::TempDir,
    store: SqliteStore,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ownership = Ownership::acquire(dir.path()).unwrap();
    let store = SqliteStore::open(ownership, "acme").await.unwrap();
    Fixture { _dir: dir, store }
}

fn key(byte: u8) -> MasterKey {
    MasterKey::from_bytes([byte; 32])
}

fn builtin(f: &Fixture, keyring: Keyring) -> BuiltinSecretStore {
    BuiltinSecretStore::new(&f.store, Arc::new(keyring), "acme")
}

#[tokio::test]
async fn values_are_ciphertext_at_rest() {
    let f = fixture().await;
    let store = builtin(&f, Keyring::new(key(1), []));
    let r = store
        .write(
            "defs/stg-redis/password",
            &SecretValue::new("hunter2-plaintext"),
        )
        .await
        .unwrap();
    assert_eq!(
        (r.store.as_str(), r.reference.as_str(), r.version.as_deref()),
        ("openporch", "defs/stg-redis/password", Some("1"))
    );

    let (key_id, ciphertext): (String, Vec<u8>) =
        sqlx::query_as("SELECT key_id, ciphertext FROM secret_versions")
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(key_id, key(1).id());
    let needle = b"hunter2-plaintext";
    assert!(!ciphertext.windows(needle.len()).any(|w| w == needle));
    // Nor anywhere else in the database file.
    f.store.pool().close().await;
    let db = std::fs::read(f._dir.path().join("openporch.db")).unwrap();
    let wal = std::fs::read(f._dir.path().join("openporch.db-wal")).unwrap_or_default();
    for bytes in [db, wal] {
        assert!(!bytes.windows(needle.len()).any(|w| w == needle));
    }
}

#[tokio::test]
async fn versions_are_kept_and_pinned() {
    let f = fixture().await;
    let store = builtin(&f, Keyring::new(key(1), []));
    let v1 = store.write("db", &SecretValue::new("one")).await.unwrap();
    let v2 = store.write("db", &SecretValue::new("two")).await.unwrap();
    assert_eq!(v2.version.as_deref(), Some("2"));
    assert_eq!(store.read(&v1).await.unwrap().expose(), "one");
    assert_eq!(store.read(&v2).await.unwrap().expose(), "two");
    let latest = SecretRef {
        version: None,
        ..v1.clone()
    };
    assert_eq!(store.read(&latest).await.unwrap().expose(), "two");

    let missing = SecretRef {
        version: Some("3".into()),
        ..v1
    };
    let e = store.read(&missing).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "secret `db` version 3 not found in store `openporch`"
    );
}

#[tokio::test]
async fn inline_values_become_references() {
    let f = fixture().await;
    let store = builtin(&f, Keyring::new(key(1), []));
    let input: SecretInput =
        serde_json::from_value(serde_json::json!({"value": "hunter2"})).unwrap();
    assert!(!format!("{input:?}").contains("hunter2"));
    let r = materialize(&store, "defs/stg-redis/password", &input)
        .await
        .unwrap();
    assert_eq!(r.version.as_deref(), Some("1"));
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::json!({"store": "openporch", "ref": "defs/stg-redis/password", "version": "1"})
    );
    // An update writes a new version and pins it.
    let input: SecretInput =
        serde_json::from_value(serde_json::json!({"value": "hunter3"})).unwrap();
    let r2 = materialize(&store, "defs/stg-redis/password", &input)
        .await
        .unwrap();
    assert_eq!(r2.version.as_deref(), Some("2"));

    // References pass through, and placeholders are refused.
    let external: SecretInput =
        serde_json::from_value(serde_json::json!({"store": "prod-sm", "ref": "shop/db"})).unwrap();
    assert_eq!(
        materialize(&store, "unused", &external)
            .await
            .unwrap()
            .store,
        "prod-sm"
    );
    let templated: SecretInput = serde_json::from_value(
        serde_json::json!({"store": "prod-sm", "ref": "${context.env.id}/db"}),
    )
    .unwrap();
    let e = materialize(&store, "unused", &templated).await.unwrap_err();
    assert!(
        e.to_string().contains("placeholders are not allowed"),
        "{e}"
    );
}

#[tokio::test]
async fn rotation_keeps_old_versions_readable() {
    let f = fixture().await;
    let old = builtin(&f, Keyring::new(key(1), []));
    let v1 = old.write("db", &SecretValue::new("one")).await.unwrap();

    let rotated = builtin(&f, Keyring::new(key(2), [key(1)]));
    let v2 = rotated.write("db", &SecretValue::new("two")).await.unwrap();
    assert_eq!(rotated.read(&v1).await.unwrap().expose(), "one");
    assert_eq!(rotated.read(&v2).await.unwrap().expose(), "two");

    let mut required = f.store.required_key_ids().await.unwrap();
    required.sort();
    let mut want = vec![key(1).id().to_owned(), key(2).id().to_owned()];
    want.sort();
    assert_eq!(required, want);

    // Without the retired key, readiness names it and old versions fail safely.
    let missing_old = Keyring::new(key(2), []);
    let e = missing_old.ensure_keys(&required).unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("master key `{}` is not available", key(1).id())
    );
    let partial = builtin(&f, missing_old);
    assert!(matches!(
        partial.read(&v1).await.unwrap_err(),
        SecretError::MissingKey { .. }
    ));
    assert_eq!(partial.read(&v2).await.unwrap().expose(), "two");
}

#[tokio::test]
async fn tampered_or_moved_ciphertext_does_not_decrypt() {
    let f = fixture().await;
    let store = builtin(&f, Keyring::new(key(1), []));
    let a = store.write("a", &SecretValue::new("alpha")).await.unwrap();
    store.write("b", &SecretValue::new("beta")).await.unwrap();
    // Copy b's ciphertext into a's row: the associated data no longer matches.
    sqlx::query(
        "UPDATE secret_versions SET nonce = (SELECT nonce FROM secret_versions WHERE ref = 'b'), \
         ciphertext = (SELECT ciphertext FROM secret_versions WHERE ref = 'b') WHERE ref = 'a'",
    )
    .execute(f.store.pool())
    .await
    .unwrap();
    let e = store.read(&a).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "secret `a` in store `openporch` could not be decrypted"
    );
}

#[test]
fn keys_load_from_base64_and_print_no_material() {
    let k = MasterKey::generate();
    let again = MasterKey::from_base64(&k.to_base64()).unwrap();
    assert_eq!(k.id(), again.id());
    assert!(k.id().starts_with("mk-"));
    assert!(!format!("{k:?}").contains(k.to_base64().as_str()));
    assert!(MasterKey::from_base64("c2hvcnQ=").is_err());
    assert_eq!(
        format!("{:?}", SecretValue::new("x")),
        "SecretValue([REDACTED])"
    );
}

#[test]
fn keyrings_load_from_text_and_owner_only_files() {
    let text = format!(
        "# active first\n{}\n\n{}\n",
        key(2).to_base64().as_str(),
        key(1).to_base64().as_str()
    );
    let ring = Keyring::from_text(&text).unwrap();
    assert_eq!(ring.active_id(), key(2).id());
    ring.ensure_keys(&[key(1).id(), key(2).id()]).unwrap();
    assert!(Keyring::from_text("# nothing\n").is_err());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("master.key");
    std::fs::write(&path, &text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = Keyring::from_file(&path).unwrap_err();
        assert!(e.to_string().contains("use 0600"), "{e}");
        assert!(!e.to_string().contains(key(2).to_base64().as_str()));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(Keyring::from_file(&path).unwrap().active_id(), key(2).id());
    assert!(
        Keyring::from_env("OPENPORCH_TEST_UNSET_KEY_VAR")
            .unwrap()
            .is_none()
    );
}
