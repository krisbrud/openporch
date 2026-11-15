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
    /// The issuer a web identity role trusts, and the subject its condition
    /// names.
    trusts: Mutex<Option<(Arc<Issuer>, String)>>,
    /// The verified claims of each web identity exchange.
    web_identity: Mutex<Vec<Value>>,
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
            trusts: Mutex::new(None),
            web_identity: Mutex::new(Vec::new()),
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

    fn assume_role_with_web_identity(
        &self,
        request: AssumeRoleWithWebIdentity,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            let (issuer, subject) = self.trusts.lock().unwrap().clone().expect("a trust");
            let now = self
                .now
                .lock()
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            // As STS does: the token verifies against the issuer's JWKS, and
            // the role's conditions name its subject and audience.
            let claims =
                crate::oidc::verify(request.token.expose(), &issuer.jwks(now), issuer.url(), now)
                    .map_err(|e| CloudError::Rejected(format!("InvalidIdentityToken: {e}")))?;
            if claims["sub"] != subject.as_str() || claims["aud"] != DEFAULT_AUDIENCE {
                return Err(CloudError::Rejected(
                    "STS AssumeRoleWithWebIdentity failed: AccessDenied: Not authorized to \
                     perform sts:AssumeRoleWithWebIdentity"
                        .into(),
                ));
            }
            self.web_identity.lock().unwrap().push(claims);
            let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(AwsCredentials {
                access_key_id: format!("ASIAWEB{n}"),
                secret_access_key: SecretValue::new(format!("web-secret-{n}")),
                session_token: Some(SecretValue::new(format!("web-token-{n}"))),
                region: None,
                expires_at: Some(*self.now.lock().unwrap() + request.duration),
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
fn ecr_images_pinned_to_digests_are_recognized() {
    let hex = "0F".repeat(32);
    let image = EcrDigest::parse(&format!(
        "111122223333.dkr.ecr-fips.us-east-1.amazonaws.com/team/web@sha256:{hex}"
    ))
    .unwrap();
    assert_eq!(
        (image.account.as_str(), image.region.as_str(), image.fips),
        ("111122223333", "us-east-1", true)
    );
    assert_eq!(image.repository, "team/web");
    assert_eq!(image.digest, format!("sha256:{}", "0f".repeat(32)));
    assert_eq!(
        image.to_string(),
        format!(
            "111122223333.dkr.ecr-fips.us-east-1.amazonaws.com/team/web@sha256:{}",
            "0f".repeat(32)
        )
    );
    // A tag before the digest is dropped.
    let tagged = EcrDigest::parse(&format!(
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web:1@sha256:{hex}"
    ))
    .unwrap();
    assert_eq!(tagged.repository, "web");
    // Tags, other registries and malformed digests are not.
    for image in [
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web:1".to_owned(),
        format!("docker.io/library/nginx@sha256:{hex}"),
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web@sha256:abc".to_owned(),
        format!("111122223333.dkr.ecr.eu-west-1.amazonaws.com/@sha256:{hex}"),
    ] {
        assert_eq!(EcrDigest::parse(&image), None, "{image}");
    }
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
    assert!(!untagged.fips);
    for (host, fips) in [
        ("111122223333.dkr.ecr-fips.us-east-1.amazonaws.com", true),
        ("111122223333.dkr-ecr.eu-west-1.on.aws", false),
        ("111122223333.dkr-ecr-fips.us-east-1.on.aws", true),
        ("111122223333.DKR.ECR.eu-west-1.AMAZONAWS.COM", false),
        ("111122223333.dkr.ecr.eu-west-1.amazonaws.com:443", false),
    ] {
        let image = EcrImage::parse(&format!("{host}/web:1")).unwrap_or_else(|| panic!("{host}"));
        assert_eq!(
            (image.account.as_str(), image.fips),
            ("111122223333", fips),
            "{host}"
        );
        assert!(in_ecr(&format!("{host}/web@sha256:abc")), "{host}");
    }

    for other in [
        "nginx:1",
        "ghcr.io/acme/web:1",
        "public.ecr.aws/acme/web:1",
        "12345.dkr.ecr.eu-west-1.amazonaws.com/web:1",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web@sha256:abc",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web:",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com/web/:1",
        "111122223333.dkr.ecr.eu-west-1.amazonaws.com:5000/web:1",
    ] {
        assert_eq!(EcrImage::parse(other), None, "{other}");
    }
    assert!(!in_ecr("ghcr.io/acme/web:1"));
    assert!(!pinned_image("nginx:1"));
    assert!(!pinned_image("nginx@sha256:abc"));
    assert!(pinned_image(&format!("nginx@sha256:{}", "AB".repeat(32))));
}

fn issuer() -> Arc<Issuer> {
    use std::sync::OnceLock;
    static KEY: OnceLock<Vec<u8>> = OnceLock::new();
    let pkcs8 = KEY.get_or_init(|| crate::oidc::generate_key().unwrap());
    Arc::new(
        Issuer::new(
            "https://porch.example.com",
            &[crate::oidc::StoredKey {
                pkcs8: pkcs8.clone(),
                created_at: 1,
                retires_at: None,
            }],
        )
        .unwrap(),
    )
}

fn identity_account(id: &str, config: Value) -> CloudAccount {
    account(id, CloudAccountType::AwsIdentity, config)
}

#[tokio::test]
async fn identity_accounts_exchange_issuer_tokens_for_role_sessions() {
    let issuer = issuer();
    let mut f = fixture();
    *f.sts.trusts.lock().unwrap() = Some((issuer.clone(), "acme/aws-prod".into()));
    f.resolver = f.resolver.with_issuer(issuer.clone(), "acme");
    let prod = identity_account(
        "aws-prod",
        json!({"role_arn": "arn:aws:iam::111122223333:role/openporch", "region": "eu-west-1"}),
    );
    let c = f.resolve(&prod, MINUTE).await.unwrap();
    assert_eq!(c.access_key_id, "ASIAWEB1");
    assert_eq!(c.region.as_deref(), Some("eu-west-1"));
    let claims = f.sts.web_identity.lock().unwrap().clone();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["sub"], "acme/aws-prod");
    assert_eq!(claims[0]["aud"], "sts.amazonaws.com");
    assert_eq!(claims[0]["iss"], "https://porch.example.com/oidc");
    let lifetime = claims[0]["exp"].as_i64().unwrap() - claims[0]["iat"].as_i64().unwrap();
    assert!(lifetime <= 15 * 60, "{lifetime}");
    // The session is reused while it stays valid; no new token is minted.
    assert_eq!(
        f.resolve(&prod, MINUTE).await.unwrap().access_key_id,
        "ASIAWEB1"
    );
    assert_eq!(f.sts.web_identity.lock().unwrap().len(), 1);
    // And replaced once it would expire within the window.
    f.advance(DEFAULT_SESSION - MINUTE);
    assert_eq!(
        f.resolve(&prod, MINUTE).await.unwrap().access_key_id,
        "ASIAWEB2"
    );

    // A role trusting another account's subject refuses this one's token,
    // and the check reports what STS said.
    let dev = identity_account(
        "aws-dev",
        json!({"role_arn": "arn:aws:iam::111122223333:role/openporch"}),
    );
    let err = f
        .resolver
        .check(&dev, f.resolver.generation(), &secrets())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CloudError::Rejected(m) if m.contains("AssumeRoleWithWebIdentity")),
        "{err}"
    );
    // A different audience is minted when the account sets one.
    let custom = identity_account(
        "aws-prod",
        json!({"role_arn": "arn:aws:iam::111122223333:role/other", "audience": "openporch"}),
    );
    f.resolver.forget("aws-prod");
    assert!(matches!(
        f.resolve(&custom, MINUTE).await,
        Err(CloudError::Rejected(_))
    ));
}

#[tokio::test]
async fn identity_accounts_need_the_issuer() {
    let f = fixture();
    let prod = identity_account(
        "aws-prod",
        json!({"role_arn": "arn:aws:iam::111122223333:role/openporch"}),
    );
    assert!(matches!(
        f.resolve(&prod, MINUTE).await,
        Err(CloudError::Invalid(m)) if m.contains("public_url")
    ));
}

/// Reads and writes Secrets Manager secrets and SSM parameters through the
/// hand-signed JSON calls, with the host's static keys. CI runs it against Floci: the job
/// creates secret `OPENPORCH_TEST_SM_SECRET` and SecureString parameter
/// `OPENPORCH_TEST_SSM_PARAMETER`, both holding `OPENPORCH_TEST_SECRET_VALUE`.
#[tokio::test]
#[ignore = "needs Secrets Manager and SSM"]
async fn real_secret_read() {
    use crate::cloud::{AwsSecret, SecretService};
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let credentials = AwsCredentials {
        access_key_id: var("AWS_ACCESS_KEY_ID"),
        secret_access_key: SecretValue::new(var("AWS_SECRET_ACCESS_KEY")),
        session_token: std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .map(SecretValue::new),
        region: Some(var("AWS_REGION")),
        expires_at: None,
    };
    let resolver = CredentialResolver::new(Arc::new(AwsSts::new()));
    let expected = var("OPENPORCH_TEST_SECRET_VALUE");
    let secret = |service, name: String, version: Option<&str>| AwsSecret {
        service,
        region: var("AWS_REGION"),
        name,
        version: version.map(str::to_owned),
    };
    for (service, name, version) in [
        (
            SecretService::SecretsManager,
            var("OPENPORCH_TEST_SM_SECRET"),
            None,
        ),
        (
            SecretService::Ssm,
            var("OPENPORCH_TEST_SSM_PARAMETER"),
            None,
        ),
        (
            SecretService::Ssm,
            var("OPENPORCH_TEST_SSM_PARAMETER"),
            Some("1"),
        ),
    ] {
        let read = resolver
            .read_secret(&credentials, &secret(service, name.clone(), version))
            .await
            .unwrap_or_else(|e| panic!("{service} `{name}`: {e}"));
        assert_eq!(
            read.expect("found").expose(),
            expected,
            "{service} `{name}`"
        );
        let missing = resolver
            .read_secret(
                &credentials,
                &secret(service, format!("{name}-missing"), None),
            )
            .await
            .unwrap_or_else(|e| panic!("{service} `{name}-missing`: {e}"));
        assert!(missing.is_none(), "{service} `{name}-missing`");
    }

    // Writing creates a secret, then adds versions; each version reads back.
    for (service, name) in [
        (
            SecretService::SecretsManager,
            format!("{}-written", var("OPENPORCH_TEST_SM_SECRET")),
        ),
        (
            SecretService::Ssm,
            format!("{}-written", var("OPENPORCH_TEST_SSM_PARAMETER")),
        ),
    ] {
        let mut versions = Vec::new();
        for (value, create) in [("first-value", true), ("second-value", false)] {
            let version = resolver
                .write_secret(
                    &credentials,
                    &secret(service, name.clone(), None),
                    &SecretValue::new(value),
                    "acme",
                    create,
                )
                .await
                .unwrap_or_else(|e| panic!("writing {service} `{name}`: {e}"));
            versions.push((version, value));
        }
        assert_ne!(versions[0].0, versions[1].0, "{service} `{name}`");
        // Creating it again is refused rather than written over.
        let error = resolver
            .write_secret(
                &credentials,
                &secret(service, name.clone(), None),
                &SecretValue::new("third-value"),
                "acme",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not created by openporch"),
            "{service}: {error}"
        );
        for (version, value) in &versions {
            let read = resolver
                .read_secret(&credentials, &secret(service, name.clone(), Some(version)))
                .await
                .unwrap_or_else(|e| panic!("{service} `{name}` version {version}: {e}"));
            assert_eq!(read.expect("found").expose(), *value, "{service} `{name}`");
        }
    }
}
