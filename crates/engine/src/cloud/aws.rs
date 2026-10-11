//! [`Sts`] on the AWS SDK. The host's own identity (environment, profile, SSO, web
//! identity, ECS or EC2 instance metadata) is loaded lazily on first use, so starting
//! the server makes no AWS calls.

use std::time::{Duration, SystemTime};

use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::Credentials;
use aws_sdk_sts::Client;
use aws_sdk_sts::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_smithy_http_client::tls::{self, rustls_provider::CryptoMode};
use openporch_store::secrets::{BoxFuture, SecretValue};
use tokio::sync::OnceCell;

use super::{AssumeRole, AwsCredentials, CallerIdentity, CloudError, Sts};

/// The region STS is called in when neither the account nor the host names one.
const FALLBACK_REGION: &str = "us-east-1";

/// How long one STS call may take, including connecting.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// STS through the AWS SDK.
#[derive(Debug, Default)]
pub struct AwsSts {
    host: OnceCell<SdkConfig>,
}

impl AwsSts {
    /// An STS client that loads the host identity on first use.
    pub fn new() -> Self {
        Self::default()
    }

    async fn host(&self) -> &SdkConfig {
        self.host
            .get_or_init(|| async {
                let http = aws_smithy_http_client::Builder::new()
                    .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
                    .build_https();
                aws_config::defaults(BehaviorVersion::latest())
                    .http_client(http)
                    .timeout_config(
                        aws_config::timeout::TimeoutConfig::builder()
                            .operation_timeout(CALL_TIMEOUT)
                            .build(),
                    )
                    .load()
                    .await
            })
            .await
    }

    /// A client in `region`, with the host identity or `credentials`.
    async fn client(&self, region: Option<&str>, credentials: Option<Credentials>) -> Client {
        let host = self.host().await;
        let region = region
            .map(|r| Region::new(r.to_owned()))
            .or_else(|| host.region().cloned())
            .unwrap_or_else(|| Region::from_static(FALLBACK_REGION));
        let mut config = aws_sdk_sts::config::Builder::from(host).region(region);
        if let Some(c) = credentials {
            config = config.credentials_provider(c);
        }
        Client::from_conf(config.build())
    }
}

/// Maps an SDK failure: a service error carries AWS's code and message, anything
/// else means AWS or the host identity could not be reached.
fn sdk_error<E, R>(what: &str, e: SdkError<E, R>) -> CloudError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    match &e {
        SdkError::ServiceError(s) => {
            let err = s.err();
            CloudError::Rejected(format!(
                "STS {what} failed: {}: {}",
                err.code().unwrap_or("Unknown"),
                err.message().unwrap_or("no message")
            ))
        }
        _ => CloudError::Unavailable(format!("STS {what} failed: {}", DisplayErrorContext(&e))),
    }
}

impl Sts for AwsSts {
    fn assume_role(
        &self,
        request: AssumeRole,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            let client = self.client(request.region.as_deref(), None).await;
            let seconds = i32::try_from(request.duration.as_secs()).unwrap_or(i32::MAX);
            let out = client
                .assume_role()
                .role_arn(&request.role_arn)
                .external_id(&request.external_id)
                .role_session_name(&request.session_name)
                .duration_seconds(seconds)
                .send()
                .await
                .map_err(|e| sdk_error("AssumeRole", e))?;
            let c = out.credentials().ok_or_else(|| {
                CloudError::Unavailable("STS AssumeRole returned no credentials".into())
            })?;
            Ok(AwsCredentials {
                access_key_id: c.access_key_id().to_owned(),
                secret_access_key: SecretValue::new(c.secret_access_key()),
                session_token: Some(SecretValue::new(c.session_token())),
                region: request.region,
                expires_at: SystemTime::try_from(*c.expiration()).ok(),
            })
        })
    }

    fn caller_identity<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>> {
        Box::pin(async move {
            let provider = Credentials::new(
                credentials.access_key_id.clone(),
                credentials.secret_access_key.expose().to_owned(),
                credentials
                    .session_token
                    .as_ref()
                    .map(|t| t.expose().to_owned()),
                credentials.expires_at,
                "openporch",
            );
            let client = self
                .client(credentials.region.as_deref(), Some(provider))
                .await;
            let out = client
                .get_caller_identity()
                .send()
                .await
                .map_err(|e| sdk_error("GetCallerIdentity", e))?;
            Ok(CallerIdentity {
                arn: out.arn().unwrap_or_default().to_owned(),
                account: out.account().unwrap_or_default().to_owned(),
                user_id: out.user_id().unwrap_or_default().to_owned(),
            })
        })
    }
}
