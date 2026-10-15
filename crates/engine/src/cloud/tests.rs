//! Credential resolution against a fake STS: static keys, role sessions with their
//! expiry and refresh, the account check and what drivers receive.

use std::sync::atomic::{AtomicU64, Ordering};

use openporch_core::ids::AccountId;
use openporch_core::model::SecretRef;
use openporch_store::secrets::SecretError;
use serde_json::{Map, json};

use super::*;

/// A fake STS. Role sessions last as long as requested unless `max_session` caps
/// them; a role whose trust policy wants another external ID is refused.
struct FakeSts {
    now: Arc<Mutex<SystemTime>>,
    calls: Mutex<Vec<AssumeRole>>,
    external_id: String,
    max_session: Mutex<Option<Duration>>,
    issued: AtomicU64,
    /// Whether GetCallerIdentity fails echoing the secret key it was given.
    echo_secret: bool,
    /// Runs while the next AssumeRole call is in flight.
    during_assume: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl FakeSts {
    fn new(now: Arc<Mutex<SystemTime>>) -> Self {
        Self {
            now,
            calls: Mutex::new(Vec::new()),
            external_id: "ext-123".into(),
            max_session: Mutex::new(None),
            issued: AtomicU64::new(0),
            echo_secret: false,
            during_assume: Mutex::new(None),
        }
    }

    fn calls(&self) -> Vec<AssumeRole> {
        self.calls.lock().unwrap().clone()
    }
}

impl Sts for FakeSts {
    fn assume_role(
        &self,
        request: AssumeRole,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.clone());
            let hook = self.during_assume.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            if request.external_id != self.external_id {
                return Err(CloudError::Rejected(
                    "STS AssumeRole failed: AccessDenied: User: arn:aws:iam::111122223333:user/host \
                     is not authorized to perform: sts:AssumeRole on resource: role/openporch"
                        .into(),
                ));
            }
            let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
            let lifetime = self
                .max_session
                .lock()
                .unwrap()
                .map_or(request.duration, |m| m.min(request.duration));
            Ok(AwsCredentials {
                access_key_id: format!("ASIASESSION{n}"),
                secret_access_key: SecretValue::new(format!("session-secret-{n}")),
                session_token: Some(SecretValue::new(format!("session-token-{n}"))),
                region: None,
                expires_at: Some(*self.now.lock().unwrap() + lifetime),
            })
        })
    }

    fn caller_identity<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>> {
        Box::pin(async move {
            if self.echo_secret {
                return Err(CloudError::Rejected(format!(
                    "SignatureDoesNotMatch: computed with {}",
                    credentials.secret_access_key.expose()
                )));
            }
            Ok(CallerIdentity {
                arn: format!(
                    "arn:aws:sts::111122223333:assumed-role/x/{}",
                    credentials.access_key_id
                ),
                account: "111122223333".into(),
                user_id: "AROAEXAMPLE:openporch".into(),
            })
        })
    }
}

/// An in-memory built-in store.
struct Secrets(HashMap<String, String>);

impl SecretStore for Secrets {
    fn id(&self) -> &str {
        "openporch"
    }

    fn write<'a>(
        &'a self,
        _: &'a str,
        _: &'a SecretValue,
    ) -> BoxFuture<'a, Result<SecretRef, SecretError>> {
        unreachable!("credential resolution never writes")
    }

    fn read<'a>(&'a self, r: &'a SecretRef) -> BoxFuture<'a, Result<SecretValue, SecretError>> {
        Box::pin(async move {
            self.0
                .get(&r.reference)
                .map(SecretValue::new)
                .ok_or_else(|| SecretError::NotFound {
                    store: r.store.clone(),
                    reference: r.reference.clone(),
                    version: r.version.clone(),
                })
        })
    }
}

fn secrets() -> Secrets {
    Secrets(HashMap::from([(
        "accounts/aws-dev/secret_access_key".into(),
        "wJalr-static-secret".into(),
    )]))
}

fn account(id: &str, ty: CloudAccountType, config: Value) -> CloudAccount {
    let config: Map<String, Value> = serde_json::from_value(config).unwrap();
    CloudAccount {
        id: AccountId::new(id).unwrap(),
        name: id.into(),
        ty,
        config,
        secret_refs: BTreeMap::new(),
    }
}

fn static_account() -> CloudAccount {
    let mut a = account(
        "aws-dev",
        CloudAccountType::Aws,
        json!({"access_key_id": "AKIAEXAMPLE", "region": "eu-north-1"}),
    );
    a.secret_refs.insert(
        "secret_access_key".into(),
        SecretRef {
            store: "openporch".into(),
            reference: "accounts/aws-dev/secret_access_key".into(),
            version: Some("1".into()),
        },
    );
    a
}

fn role_account(config: Value) -> CloudAccount {
    account("aws-prod", CloudAccountType::AwsRole, config)
}

fn role_config() -> Value {
    json!({"role_arn": "arn:aws:iam::111122223333:role/openporch", "external_id": "ext-123",
           "region": "eu-west-1"})
}

struct Fixture {
    now: Arc<Mutex<SystemTime>>,
    sts: Arc<FakeSts>,
    resolver: CredentialResolver,
}

fn fixture_with(sts: impl FnOnce(&mut FakeSts)) -> Fixture {
    let now = Arc::new(Mutex::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    ));
    let mut fake = FakeSts::new(now.clone());
    sts(&mut fake);
    let sts = Arc::new(fake);
    let clock_now = now.clone();
    let resolver =
        CredentialResolver::with_clock(sts.clone(), Arc::new(move || *clock_now.lock().unwrap()));
    Fixture { now, sts, resolver }
}

fn fixture() -> Fixture {
    fixture_with(|_| {})
}

impl Fixture {
    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }

    async fn resolve(
        &self,
        a: &CloudAccount,
        window: Duration,
    ) -> Result<AwsCredentials, CloudError> {
        self.resolver
            .resolve(a, self.resolver.generation(), &secrets(), window)
            .await
    }
}

const MINUTE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn static_keys_come_from_the_secret_reference() {
    let f = fixture();
    let c = f.resolve(&static_account(), MINUTE).await.unwrap();
    assert_eq!(c.access_key_id, "AKIAEXAMPLE");
    assert_eq!(c.secret_access_key.expose(), "wJalr-static-secret");
    assert!(c.session_token.is_none() && c.expires_at.is_none());
    // Static keys do not expire, whatever the window.
    assert!(
        f.resolve(&static_account(), Duration::from_secs(86_400))
            .await
            .is_ok()
    );
    assert!(f.sts.calls().is_empty());

    // References to stores this server cannot read yet fail without a value.
    let mut other = static_account();
    other
        .secret_refs
        .get_mut("secret_access_key")
        .unwrap()
        .store = "prod-sm".into();
    let err = f.resolve(&other, MINUTE).await.unwrap_err();
    assert!(
        matches!(&err, CloudError::Unavailable(m) if m.contains("prod-sm")),
        "{err}"
    );
    let mut missing = static_account();
    missing
        .secret_refs
        .get_mut("secret_access_key")
        .unwrap()
        .reference = "gone".into();
    assert!(matches!(
        f.resolve(&missing, MINUTE).await,
        Err(CloudError::Unavailable(_))
    ));
}

#[tokio::test]
async fn drivers_receive_only_the_account_credentials() {
    let f = fixture();
    let c = f.resolve(&static_account(), MINUTE).await.unwrap();
    let inputs = c.driver_inputs();
    assert_eq!(inputs.keys().copied().collect::<Vec<_>>(), {
        let mut k = DRIVER_CREDENTIAL_KEYS.to_vec();
        k.sort_unstable();
        k
    });
    assert_eq!(inputs["AccessKeyId"].expose(), "AKIAEXAMPLE");
    assert_eq!(inputs["SecretAccessKey"].expose(), "wJalr-static-secret");
    assert_eq!(inputs["SessionToken"].expose(), "");
    assert_eq!(inputs["Region"].expose(), "eu-north-1");

    // A role account injects the assumed session, never the host identity.
    let r = f
        .resolve(&role_account(role_config()), MINUTE)
        .await
        .unwrap();
    let inputs = r.driver_inputs();
    assert_eq!(inputs.len(), DRIVER_CREDENTIAL_KEYS.len());
    assert_eq!(inputs["AccessKeyId"].expose(), "ASIASESSION1");
    assert_eq!(inputs["SessionToken"].expose(), "session-token-1");
    assert_eq!(inputs["Region"].expose(), "eu-west-1");

    // Credentials never print.
    for text in [format!("{c:?}"), format!("{r:?}"), format!("{inputs:?}")] {
        for secret in ["wJalr-static-secret", "session-secret-1", "session-token-1"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
}

#[tokio::test]
async fn role_accounts_assume_the_role_with_the_external_id() {
    let f = fixture();
    let a = role_account(role_config());
    f.resolve(&a, MINUTE).await.unwrap();
    let calls = f.sts.calls();
    assert_eq!(
        calls,
        [AssumeRole {
            role_arn: "arn:aws:iam::111122223333:role/openporch".into(),
            external_id: "ext-123".into(),
            session_name: "openporch-aws-prod".into(),
            duration: DEFAULT_SESSION,
            region: Some("eu-west-1".into()),
        }]
    );

    // Wrong external ID: the STS failure message is returned.
    let wrong = role_account(
        json!({"role_arn": "arn:aws:iam::111122223333:role/openporch",
                                    "external_id": "other"}),
    );
    let err = f
        .resolver
        .check(&wrong, f.resolver.generation(), &secrets())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CloudError::Rejected(m) if m.contains("AccessDenied")),
        "{err}"
    );
}

#[tokio::test]
async fn role_sessions_are_reused_and_refreshed_before_expiry() {
    let f = fixture();
    let a = role_account(
        json!({"role_arn": "arn:aws:iam::1:role/r", "external_id": "ext-123",
                                "session_duration": 900}),
    );
    let window = Duration::from_secs(300);
    let first = f.resolve(&a, window).await.unwrap();
    // Still valid for the window plus margin: reused.
    f.advance(Duration::from_secs(500));
    let second = f.resolve(&a, window).await.unwrap();
    assert_eq!(second.access_key_id, first.access_key_id);
    assert_eq!(f.sts.calls().len(), 1);
    // Too close to expiry for another window: a new session is assumed.
    f.advance(Duration::from_secs(100));
    let third = f.resolve(&a, window).await.unwrap();
    assert_ne!(third.access_key_id, first.access_key_id);
    assert_eq!(f.sts.calls().len(), 2);
    assert!(third.valid_for(*f.now.lock().unwrap(), window));

    // Expired sessions of other accounts are dropped when a session is stored.
    let mut other = a.clone();
    other.id = AccountId::new("aws-other").unwrap();
    f.resolve(&other, window).await.unwrap();
    assert_eq!(f.resolver.sessions().len(), 2);
    f.advance(Duration::from_secs(1000));
    f.resolve(&other, window).await.unwrap();
    assert_eq!(f.resolver.sessions().len(), 1);
    assert_eq!(f.sts.calls().len(), 4);

    // Expired sessions are also dropped on their own, and deleted accounts forgotten.
    f.advance(Duration::from_secs(1000));
    f.resolver.prune();
    assert!(f.resolver.sessions().is_empty());
    f.resolve(&other, window).await.unwrap();
    f.resolver.forget("aws-other");
    assert!(f.resolver.sessions().is_empty());

    // A changed account configuration never reuses the old session.
    let mut changed = a.clone();
    changed.config.insert("region".into(), json!("eu-west-1"));
    f.resolve(&changed, window).await.unwrap();
    assert_eq!(f.sts.calls().len(), 6);
}

#[tokio::test]
async fn a_session_obtained_while_its_account_changes_is_not_cached() {
    let now = Arc::new(Mutex::new(SystemTime::UNIX_EPOCH));
    let sts = Arc::new(FakeSts::new(now.clone()));
    let resolver = Arc::new(CredentialResolver::with_clock(
        sts.clone(),
        Arc::new(move || *now.lock().unwrap()),
    ));
    let weak = Arc::downgrade(&resolver);
    *sts.during_assume.lock().unwrap() = Some(Box::new(move || {
        if let Some(r) = weak.upgrade() {
            r.forget("aws-prod");
        }
    }));
    let a = role_account(role_config());
    // Read before a concurrent change: the session is returned but not cached.
    let read_at = resolver.generation();
    let first = resolver
        .resolve(&a, read_at, &secrets(), MINUTE)
        .await
        .unwrap();
    assert!(resolver.sessions().is_empty());
    // An account read before the change, even if the call starts after it, is
    // not cached either.
    let third = resolver
        .resolve(&a, read_at, &secrets(), MINUTE)
        .await
        .unwrap();
    assert!(resolver.sessions().is_empty());
    assert_ne!(first.access_key_id, third.access_key_id);
    let second = resolver
        .resolve(&a, resolver.generation(), &secrets(), MINUTE)
        .await
        .unwrap();
    assert_ne!(first.access_key_id, second.access_key_id);
    assert_eq!(resolver.sessions().len(), 1);
}

#[tokio::test]
async fn work_longer_than_a_session_fails_before_any_call() {
    let f = fixture();
    let a = role_account(
        json!({"role_arn": "arn:aws:iam::1:role/r", "external_id": "ext-123",
                                "session_duration": 900}),
    );
    let err = f.resolve(&a, Duration::from_secs(1800)).await.unwrap_err();
    assert!(
        matches!(&err, CloudError::Expiring(m) if m.contains("session_duration")),
        "{err}"
    );
    assert!(f.sts.calls().is_empty());

    // STS issuing a shorter session than requested is caught too.
    let f = fixture_with(|s| *s.max_session.get_mut().unwrap() = Some(Duration::from_secs(900)));
    let a = role_account(role_config());
    let err = f.resolve(&a, Duration::from_secs(1800)).await.unwrap_err();
    assert!(matches!(err, CloudError::Expiring(_)), "{err}");
}

#[tokio::test]
async fn check_reports_the_caller_identity() {
    let f = fixture();
    let id = f
        .resolver
        .check(&static_account(), f.resolver.generation(), &secrets())
        .await
        .unwrap();
    assert_eq!(id.account, "111122223333");
    assert!(id.arn.ends_with("AKIAEXAMPLE"), "{id:?}");
    let id = f
        .resolver
        .check(
            &role_account(role_config()),
            f.resolver.generation(),
            &secrets(),
        )
        .await
        .unwrap();
    assert!(id.arn.ends_with("ASIASESSION1"), "{id:?}");

    // Echoed secrets are redacted from failures.
    let f = fixture_with(|s| s.echo_secret = true);
    let err = f
        .resolver
        .check(&static_account(), f.resolver.generation(), &secrets())
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("wJalr-static-secret"), "{err}");
    assert!(err.to_string().contains("[REDACTED]"), "{err}");

    let identity = account("aws-oidc", CloudAccountType::AwsIdentity, json!({}));
    assert!(matches!(
        f.resolve(&identity, MINUTE).await,
        Err(CloudError::Invalid(m)) if m.contains("OIDC")
    ));
}

/// The documented real account check (see `docs/cloud-accounts.md`): assumes a real
/// role with the host's identity and asks STS who it is. Run with `just e2e` after
/// setting `OPENPORCH_TEST_AWS_ROLE_ARN` and `OPENPORCH_TEST_AWS_EXTERNAL_ID`.
#[tokio::test]
#[ignore = "needs AWS credentials and a test role"]
async fn real_account_check() {
    let (Ok(role_arn), Ok(external_id)) = (
        std::env::var("OPENPORCH_TEST_AWS_ROLE_ARN"),
        std::env::var("OPENPORCH_TEST_AWS_EXTERNAL_ID"),
    ) else {
        panic!("set OPENPORCH_TEST_AWS_ROLE_ARN and OPENPORCH_TEST_AWS_EXTERNAL_ID");
    };
    let resolver = CredentialResolver::new(Arc::new(AwsSts::new()));
    let a = role_account(json!({"role_arn": role_arn, "external_id": external_id,
                                "session_duration": 900}));
    let id = resolver
        .check(&a, resolver.generation(), &secrets())
        .await
        .unwrap();
    assert!(id.arn.contains(":assumed-role/"), "{id:?}");
    assert!(id.arn.ends_with("/openporch-aws-prod"), "{id:?}");

    let wrong = role_account(json!({"role_arn": a.config["role_arn"], "external_id": "wrong"}));
    let err = resolver
        .check(&wrong, resolver.generation(), &secrets())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CloudError::Rejected(m) if m.contains("AccessDenied")),
        "{err}"
    );
}

#[test]
fn only_an_s3_object_with_a_version_is_an_s3_version() {
    let v = S3Version::parse("s3://artifacts/order-worker/abc.zip?versionId=3HL4kqtJ").unwrap();
    assert_eq!(
        (v.bucket.as_str(), v.key.as_str(), v.version.as_str()),
        ("artifacts", "order-worker/abc.zip", "3HL4kqtJ")
    );
    assert_eq!(
        v.to_string(),
        "s3://artifacts/order-worker/abc.zip?versionId=3HL4kqtJ"
    );
    // Keys are percent-decoded, as S3 names the object.
    let v = S3Version::parse("s3://artifacts/my%20worker.zip?versionId=a%2Bb").unwrap();
    assert_eq!(
        (v.key.as_str(), v.version.as_str()),
        ("my worker.zip", "a+b")
    );
    for uri in [
        "s3://artifacts/latest.zip",
        "s3://artifacts/latest.zip?versionId=",
        "s3:///k?versionId=v",
        "s3://artifacts?versionId=v",
        "https://artifacts.s3.amazonaws.com/k?versionId=v",
        ".",
        "s3://artifacts/k%zz?versionId=v",
        "s3://artifacts/k%+1?versionId=v",
        "s3://artifacts/k?versionId=v#fragment",
    ] {
        assert_eq!(S3Version::parse(uri), None, "{uri}");
    }
}

#[test]
fn an_s3_object_without_a_version_round_trips() {
    let o = S3Object::parse("s3://artifacts/a%3Fb%25c%23d.zip").unwrap();
    assert_eq!(o.key, "a?b%c#d.zip");
    let pinned = o.at("v&1".into()).to_string();
    assert_eq!(
        S3Version::parse(&pinned),
        Some(S3Version {
            bucket: "artifacts".into(),
            key: "a?b%c#d.zip".into(),
            version: "v&1".into()
        }),
        "{pinned}"
    );
    assert_eq!(S3Object::parse(&o.to_string()), Some(o));
    for uri in [
        "s3://artifacts/k?versionId=v",
        "s3://artifacts/",
        "s3://artifacts",
        "s3:///k",
        "s3://artifacts/k#f",
        "https://example.com/k",
    ] {
        assert_eq!(S3Object::parse(uri), None, "{uri}");
    }
}

/// Checks object versions in a real or emulated versioned bucket with the
/// host's identity, and pins objects to their current version. CI's Floci job
/// uploads the object and sets `OPENPORCH_TEST_ARTIFACT_BUCKET`,
/// `OPENPORCH_TEST_ARTIFACT_KEY` and `OPENPORCH_TEST_ARTIFACT_VERSION`, and
/// puts the same key in the unversioned `OPENPORCH_TEST_PLAIN_BUCKET`.
#[tokio::test]
#[ignore = "needs S3 and a versioned bucket"]
async fn real_artifact_check() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let object = S3Version {
        bucket: var("OPENPORCH_TEST_ARTIFACT_BUCKET"),
        key: var("OPENPORCH_TEST_ARTIFACT_KEY"),
        version: var("OPENPORCH_TEST_ARTIFACT_VERSION"),
    };
    let resolver = CredentialResolver::new(Arc::new(AwsSts::new()));
    assert!(resolver.object_exists(None, &object).await.unwrap());
    let missing = S3Version {
        key: format!("{}.missing", object.key),
        ..object.clone()
    };
    assert!(!resolver.object_exists(None, &missing).await.unwrap());

    // The current version pins the object; an unversioned bucket has none.
    let object = S3Object {
        bucket: object.bucket.clone(),
        key: object.key.clone(),
    };
    assert_eq!(
        resolver.latest_version(None, &object).await.unwrap(),
        Latest::Version(var("OPENPORCH_TEST_ARTIFACT_VERSION"))
    );
    let missing = S3Object {
        key: format!("{}.missing", object.key),
        ..object.clone()
    };
    assert_eq!(
        resolver.latest_version(None, &missing).await.unwrap(),
        Latest::Missing
    );
    let plain = S3Object {
        bucket: var("OPENPORCH_TEST_PLAIN_BUCKET"),
        key: object.key.clone(),
    };
    assert_eq!(
        resolver.latest_version(None, &plain).await.unwrap(),
        Latest::Unversioned
    );
}

#[test]
fn ecr_images_named_by_tag_are_recognized() {
    let image =
        EcrImage::parse("111122223333.dkr.ecr.eu-west-1.amazonaws.com/team/web:1.2").unwrap();
    assert_eq!(image.account, "111122223333");
    assert_eq!(image.region, "eu-west-1");
    assert_eq!(image.repository, "team/web");
    assert_eq!(image.tag, "1.2");
    let digest = format!("sha256:{}", "0f".repeat(32));
    assert_eq!(
        image.at(&digest),
        format!("111122223333.dkr.ecr.eu-west-1.amazonaws.com/team/web@{digest}")
    );
    assert!(pinned_image(&image.at(&digest)));

    let untagged = EcrImage::parse("111122223333.dkr.ecr.cn-north-1.amazonaws.com.cn/web").unwrap();
    assert_eq!(untagged.tag, "latest");

    for other in [
        "nginx:1",
        "ghcr.io/acme/web:1",
        "public.ecr.aws/acme/web:1",
        "12345.dkr.ecr.eu-west-1.amazonaws.com/web:1",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web@sha256:abc",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web:",
    ] {
        assert_eq!(EcrImage::parse(other), None, "{other}");
    }
    assert!(!pinned_image("nginx:1"));
    assert!(!pinned_image("nginx@sha256:abc"));
    assert!(pinned_image(&format!("nginx@sha256:{}", "AB".repeat(32))));
}
