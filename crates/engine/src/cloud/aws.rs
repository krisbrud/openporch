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

use super::{
    AssumeRole, AwsCredentials, CallerIdentity, CloudError, EcrImage, Latest, S3Object, S3Version,
    Sts,
};

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

impl AwsSts {
    /// An S3 client in `region`, with the host identity or `credentials`.
    /// An endpoint set for the host, as for an emulator, is addressed by path.
    async fn s3(
        &self,
        region: Option<&str>,
        credentials: Option<Credentials>,
    ) -> aws_sdk_s3::Client {
        let host = self.host().await;
        let region = region
            .map(|r| Region::new(r.to_owned()))
            .or_else(|| host.region().cloned())
            .unwrap_or_else(|| Region::from_static(FALLBACK_REGION));
        let mut config = aws_sdk_s3::config::Builder::from(host)
            .region(region)
            .force_path_style(host.endpoint_url().is_some());
        if let Some(c) = credentials {
            config = config.credentials_provider(c);
        }
        aws_sdk_s3::Client::from_conf(config.build())
    }

    /// An ECR client for `image`'s region, through a FIPS endpoint for a
    /// FIPS registry, with `credentials`.
    async fn ecr(&self, image: &EcrImage, credentials: Credentials) -> aws_sdk_ecr::Client {
        let host = self.host().await;
        let mut config = aws_sdk_ecr::config::Builder::from(host)
            .region(Region::new(image.region.clone()))
            .credentials_provider(credentials);
        if image.fips {
            config = config.use_fips(true);
        }
        aws_sdk_ecr::Client::from_conf(config.build())
    }
}

/// The SDK's credentials for `credentials`.
fn provider(credentials: &AwsCredentials) -> Credentials {
    Credentials::new(
        credentials.access_key_id.clone(),
        credentials.secret_access_key.expose().to_owned(),
        credentials
            .session_token
            .as_ref()
            .map(|t| t.expose().to_owned()),
        credentials.expires_at,
        "openporch",
    )
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
            let client = self
                .client(credentials.region.as_deref(), Some(provider(credentials)))
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

    fn object_exists<'a>(
        &'a self,
        credentials: Option<&'a AwsCredentials>,
        object: &'a S3Version,
    ) -> BoxFuture<'a, Result<bool, CloudError>> {
        Box::pin(async move {
            let name = object.to_string();
            let head = self
                .head(
                    credentials,
                    &object.bucket,
                    &object.key,
                    Some(&object.version),
                    &name,
                )
                .await?;
            Ok(head.is_some())
        })
    }

    fn latest_version<'a>(
        &'a self,
        credentials: Option<&'a AwsCredentials>,
        object: &'a S3Object,
    ) -> BoxFuture<'a, Result<Latest, CloudError>> {
        Box::pin(async move {
            let name = object.to_string();
            let head = self
                .head(credentials, &object.bucket, &object.key, None, &name)
                .await?;
            Ok(match head {
                None => Latest::Missing,
                // An unversioned bucket reports no version, or `null`.
                Some(Some(v)) if v != "null" => Latest::Version(v),
                Some(_) => Latest::Unversioned,
            })
        })
    }
    fn image_digest<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        image: &'a EcrImage,
    ) -> BoxFuture<'a, Result<Option<String>, CloudError>> {
        Box::pin(async move {
            let client = self.ecr(image, provider(credentials)).await;
            let id = aws_sdk_ecr::types::ImageIdentifier::builder()
                .image_tag(&image.tag)
                .build();
            let out = match client
                .describe_images()
                .registry_id(&image.account)
                .repository_name(&image.repository)
                .image_ids(id)
                .send()
                .await
            {
                Ok(out) => out,
                Err(SdkError::ServiceError(e))
                    if matches!(
                        e.err().code(),
                        Some("ImageNotFoundException" | "RepositoryNotFoundException")
                    ) =>
                {
                    return Ok(None);
                }
                Err(SdkError::ServiceError(e)) => {
                    let err = e.err();
                    return Err(CloudError::Rejected(format!(
                        "ECR DescribeImages of `{image}` failed: {}: {}",
                        err.code().unwrap_or("Unknown"),
                        err.message().unwrap_or("no message")
                    )));
                }
                Err(e) => {
                    return Err(CloudError::Unavailable(format!(
                        "ECR DescribeImages of `{image}` failed: {}",
                        DisplayErrorContext(&e)
                    )));
                }
            };
            Ok(out
                .image_details()
                .first()
                .and_then(|d| d.image_digest())
                .map(str::to_owned))
        })
    }
}

impl AwsSts {
    /// HeadObject on `key` (at `version`): `None` when there is no such
    /// object or version, otherwise the version ID S3 reports.
    async fn head(
        &self,
        credentials: Option<&AwsCredentials>,
        bucket: &str,
        key: &str,
        version: Option<&str>,
        name: &str,
    ) -> Result<Option<Option<String>>, CloudError> {
        let mut region = credentials.and_then(|c| c.region.clone());
        // A bucket in another region than the one asked answers with its
        // own, and is asked again there once.
        for _ in 0..2 {
            let client = self.s3(region.as_deref(), credentials.map(provider)).await;
            let out = client
                .head_object()
                .bucket(bucket)
                .key(key)
                .set_version_id(version.map(str::to_owned))
                .send()
                .await;
            let s = match out {
                Ok(out) => return Ok(Some(out.version_id().map(str::to_owned))),
                Err(SdkError::ServiceError(s)) => s,
                Err(e) => {
                    return Err(CloudError::Unavailable(format!(
                        "S3 HeadObject on `{name}` failed: {}",
                        DisplayErrorContext(&e)
                    )));
                }
            };
            let status = s.raw().status().as_u16();
            let bucket_region = s
                .raw()
                .headers()
                .get("x-amz-bucket-region")
                .map(str::to_owned);
            match status {
                // No such object or version; 405 is a version that is a
                // delete marker.
                404 | 405 => return Ok(None),
                301 | 400 if bucket_region.is_some() && bucket_region != region => {
                    region = bucket_region;
                }
                403 => {
                    return Err(CloudError::Rejected(format!(
                        "S3 HeadObject on `{name}` was denied; without `s3:ListBucket` \
                         S3 also denies reading an object that does not exist"
                    )));
                }
                500.. => {
                    return Err(CloudError::Unavailable(format!(
                        "S3 HeadObject on `{name}` failed with status {status}"
                    )));
                }
                _ => {
                    return Err(CloudError::Rejected(format!(
                        "S3 HeadObject on `{name}` failed with status {status}{}; check \
                         the version ID and the account's region",
                        bucket_region
                            .map(|r| format!(" (the bucket is in {r})"))
                            .unwrap_or_default()
                    )));
                }
            }
        }
        Err(CloudError::Rejected(format!(
            "S3 HeadObject on `{name}` was redirected more than once"
        )))
    }
}
