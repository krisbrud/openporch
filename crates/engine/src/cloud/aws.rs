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
    AssumeRole, AssumeRoleWithWebIdentity, AwsCredentials, AwsSecret, CallerIdentity, CloudError,
    EcrDigest, EcrImage, Latest, S3Object, S3Version, SecretService, Sts,
};

/// The region STS is called in when neither the account nor the host names one.
const FALLBACK_REGION: &str = "us-east-1";

/// How long one STS call may take, including connecting.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// STS through the AWS SDK.
#[derive(Debug, Default)]
pub struct AwsSts {
    host: OnceCell<SdkConfig>,
    /// Calls the JSON APIs of Secrets Manager and SSM, which are signed here.
    http: std::sync::OnceLock<reqwest::Client>,
}

/// The largest secret response read: Secrets Manager holds up to 64 KiB, and
/// the JSON around it and its encoding add some.
const MAX_SECRET_RESPONSE: usize = 256 * 1024;

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

    /// An ECR client for `region`, through a FIPS endpoint for a FIPS
    /// registry, with `credentials`.
    async fn ecr(&self, region: &str, fips: bool, credentials: Credentials) -> aws_sdk_ecr::Client {
        let host = self.host().await;
        let mut config = aws_sdk_ecr::config::Builder::from(host)
            .region(Region::new(region.to_owned()))
            .credentials_provider(credentials);
        if fips {
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
    fn read_secret<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        secret: &'a AwsSecret,
    ) -> BoxFuture<'a, Result<Option<SecretValue>, CloudError>> {
        Box::pin(async move { self.read_secret_json(credentials, secret).await })
    }

    fn write_secret<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        secret: &'a AwsSecret,
        value: &'a SecretValue,
        org: &'a str,
        create: bool,
    ) -> BoxFuture<'a, Result<String, CloudError>> {
        Box::pin(async move {
            self.write_secret_json(credentials, secret, value, org, create)
                .await
        })
    }

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

    fn assume_role_with_web_identity(
        &self,
        request: AssumeRoleWithWebIdentity,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            // The operation is unsigned, so the host needs no identity of its own.
            let client = self.client(request.region.as_deref(), None).await;
            let seconds = i32::try_from(request.duration.as_secs()).unwrap_or(i32::MAX);
            let out = client
                .assume_role_with_web_identity()
                .role_arn(&request.role_arn)
                .web_identity_token(request.token.expose())
                .role_session_name(&request.session_name)
                .duration_seconds(seconds)
                .send()
                .await
                .map_err(|e| sdk_error("AssumeRoleWithWebIdentity", e))?;
            let c = out.credentials().ok_or_else(|| {
                CloudError::Unavailable(
                    "STS AssumeRoleWithWebIdentity returned no credentials".into(),
                )
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
            let client = self
                .ecr(&image.region, image.fips, provider(credentials))
                .await;
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

    fn image_exists<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
        image: &'a EcrDigest,
    ) -> BoxFuture<'a, Result<bool, CloudError>> {
        Box::pin(async move {
            let client = self
                .ecr(&image.region, image.fips, provider(credentials))
                .await;
            let id = aws_sdk_ecr::types::ImageIdentifier::builder()
                .image_digest(&image.digest)
                .build();
            match client
                .describe_images()
                .registry_id(&image.account)
                .repository_name(&image.repository)
                .image_ids(id)
                .send()
                .await
            {
                Ok(out) => Ok(!out.image_details().is_empty()),
                Err(SdkError::ServiceError(e))
                    if matches!(
                        e.err().code(),
                        Some("ImageNotFoundException" | "RepositoryNotFoundException")
                    ) =>
                {
                    Ok(false)
                }
                Err(SdkError::ServiceError(e)) => {
                    let err = e.err();
                    Err(CloudError::Rejected(format!(
                        "ECR DescribeImages of `{image}` failed: {}: {}",
                        err.code().unwrap_or("Unknown"),
                        err.message().unwrap_or("no message")
                    )))
                }
                Err(e) => Err(CloudError::Unavailable(format!(
                    "ECR DescribeImages of `{image}` failed: {}",
                    DisplayErrorContext(&e)
                ))),
            }
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

impl AwsSts {
    /// A JSON 1.1 call to `service` (`secretsmanager` or `ssm`) in `region`,
    /// signed with SigV4. A host endpoint, as for an emulator, is used for both.
    async fn json_call(
        &self,
        credentials: &AwsCredentials,
        service: &str,
        target: &str,
        region: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, zeroize::Zeroizing<Vec<u8>>), CloudError> {
        use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
        use aws_sigv4::sign::v4;

        let host = self.host().await;
        let url = match host.endpoint_url() {
            Some(endpoint) => format!("{}/", endpoint.trim_end_matches('/')),
            // The China partition has its own domain.
            None if region.starts_with("cn-") => {
                format!("https://{service}.{region}.amazonaws.com.cn/")
            }
            None => format!("https://{service}.{region}.amazonaws.com/"),
        };
        let body = serde_json::to_vec(body).map_err(|e| CloudError::Invalid(e.to_string()))?;
        let headers = [
            ("content-type", "application/x-amz-json-1.1"),
            ("x-amz-target", target),
        ];
        let identity = provider(credentials).into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(region)
            .name(service)
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| CloudError::Invalid(format!("signing a {target} request: {e}")))?
            .into();
        let signable = SignableRequest::new(
            "POST",
            &url,
            headers.iter().copied(),
            SignableBody::Bytes(&body),
        )
        .map_err(|e| CloudError::Invalid(format!("signing a {target} request: {e}")))?;
        let (instructions, _) = sign(signable, &params)
            .map_err(|e| CloudError::Invalid(format!("signing a {target} request: {e}")))?
            .into_parts();
        let client = self.http.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .timeout(CALL_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default()
        });
        let mut req = client.post(&url).body(body);
        for (name, value) in headers.iter().copied().chain(instructions.headers()) {
            req = req.header(name, value);
        }
        let mut res = req
            .send()
            .await
            .map_err(|e| CloudError::Unavailable(format!("{target} could not reach {url}: {e}")))?;
        let status = res.status().as_u16();
        // The response may hold a secret, so even a partial one is zeroed.
        let mut out = zeroize::Zeroizing::new(Vec::new());
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| CloudError::Unavailable(format!("{target} failed: {e}")))?
        {
            if out.len() + chunk.len() > MAX_SECRET_RESPONSE {
                return Err(CloudError::Rejected(format!(
                    "{target} answered with more than {MAX_SECRET_RESPONSE} bytes"
                )));
            }
            out.extend_from_slice(&chunk);
        }
        Ok((status, out))
    }

    async fn read_secret_json(
        &self,
        credentials: &AwsCredentials,
        secret: &AwsSecret,
    ) -> Result<Option<SecretValue>, CloudError> {
        let (service, target, body) = match secret.service {
            SecretService::SecretsManager => {
                let mut body = serde_json::json!({ "SecretId": secret.name });
                if let Some(v) = &secret.version {
                    body["VersionId"] = v.clone().into();
                }
                ("secretsmanager", "secretsmanager.GetSecretValue", body)
            }
            SecretService::Ssm => {
                let name = match &secret.version {
                    Some(v) => format!("{}:{v}", secret.name),
                    None => secret.name.clone(),
                };
                (
                    "ssm",
                    "AmazonSSM.GetParameter",
                    serde_json::json!({ "Name": name, "WithDecryption": true }),
                )
            }
        };
        let (status, out) = self
            .json_call(credentials, service, target, &secret.region, &body)
            .await?;
        if !(200..300).contains(&status) {
            // An error response holds no secret.
            let value: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
            let (code, message) = aws_error(&value);
            return match code {
                "ResourceNotFoundException" | "ParameterNotFound" | "ParameterVersionNotFound" => {
                    Ok(None)
                }
                _ => Err(call_error(target, secret, status, code, message)),
            };
        }
        // The value is parsed straight into a `SecretValue`, which is zeroed on
        // drop, so no other copy of it is left behind.
        let no_value = || CloudError::Rejected(format!("{target} of {secret} returned no value"));
        let text = match secret.service {
            SecretService::SecretsManager => {
                let answer: SecretsManagerValue =
                    serde_json::from_slice(&out).map_err(|_| no_value())?;
                if answer.secret_binary.is_some() && answer.secret_string.is_none() {
                    return Err(CloudError::Rejected(format!(
                        "{secret} holds a binary secret; only text secrets can be read"
                    )));
                }
                answer.secret_string
            }
            SecretService::Ssm => serde_json::from_slice::<SsmParameterValue>(&out)
                .map_err(|_| no_value())?
                .parameter
                .and_then(|p| p.value),
        };
        text.map(Some).ok_or_else(no_value)
    }
}

/// A `GetSecretValue` answer, as far as it is read.
#[derive(serde::Deserialize)]
struct SecretsManagerValue {
    #[serde(rename = "SecretString")]
    secret_string: Option<SecretValue>,
    #[serde(rename = "SecretBinary")]
    secret_binary: Option<serde::de::IgnoredAny>,
}

/// A `GetParameter` answer, as far as it is read.
#[derive(serde::Deserialize)]
struct SsmParameterValue {
    #[serde(rename = "Parameter")]
    parameter: Option<SsmParameter>,
}

#[derive(serde::Deserialize)]
struct SsmParameter {
    #[serde(rename = "Value")]
    value: Option<SecretValue>,
}

impl AwsSts {
    /// Writes `value` as a new version of `secret`, creating it, tagged as
    /// written by openporch for `org`, when it does not exist. Returns the new
    /// version.
    pub(super) async fn write_secret_json(
        &self,
        credentials: &AwsCredentials,
        secret: &AwsSecret,
        value: &SecretValue,
        org: &str,
        create: bool,
    ) -> Result<String, CloudError> {
        let tags = serde_json::json!([
            {"Key": "openporch:managed", "Value": "true"},
            {"Key": "openporch:org", "Value": org},
        ]);
        let (service, target, body) = match (secret.service, create) {
            (SecretService::SecretsManager, true) => (
                "secretsmanager",
                "secretsmanager.CreateSecret",
                serde_json::json!({
                    "Name": secret.name,
                    "SecretString": value.expose(),
                    "Description": "Written by openporch",
                    "Tags": tags,
                }),
            ),
            (SecretService::SecretsManager, false) => (
                "secretsmanager",
                "secretsmanager.PutSecretValue",
                serde_json::json!({"SecretId": secret.name, "SecretString": value.expose()}),
            ),
            (SecretService::Ssm, true) => (
                "ssm",
                "AmazonSSM.PutParameter",
                serde_json::json!({
                    "Name": secret.name, "Value": value.expose(),
                    "Type": "SecureString", "Tags": tags,
                }),
            ),
            (SecretService::Ssm, false) => (
                "ssm",
                "AmazonSSM.PutParameter",
                serde_json::json!({
                    "Name": secret.name, "Value": value.expose(),
                    "Type": "SecureString", "Overwrite": true,
                }),
            ),
        };
        let (status, out) = self
            .json_call(credentials, service, target, &secret.region, &body)
            .await?;
        let answer: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
        if !(200..300).contains(&status) {
            let (code, message) = aws_error(&answer);
            if create && matches!(code, "ResourceExistsException" | "ParameterAlreadyExists") {
                // Someone else put a secret at this name: it is not written
                // over, and not openporch's.
                return Err(CloudError::Rejected(format!(
                    "{secret} already exists and was not created by openporch, so it is not \
                     written over"
                )));
            }
            return Err(call_error(target, secret, status, code, message));
        }
        let version = match secret.service {
            SecretService::SecretsManager => answer["VersionId"].as_str().map(str::to_owned),
            SecretService::Ssm => answer["Version"].as_u64().map(|v| v.to_string()),
        };
        version.ok_or_else(|| {
            CloudError::Rejected(format!("{target} of {secret} returned no version"))
        })
    }
}

/// The error code and message of a JSON 1.1 error response.
fn aws_error(value: &serde_json::Value) -> (&str, &str) {
    let code = value["__type"]
        .as_str()
        .map(|t| t.rsplit('#').next().unwrap_or(t))
        .unwrap_or("Unknown");
    let message = value["message"]
        .as_str()
        .or_else(|| value["Message"].as_str())
        .unwrap_or("no message");
    (code, message)
}

fn call_error(
    target: &str,
    secret: &AwsSecret,
    status: u16,
    code: &str,
    message: &str,
) -> CloudError {
    match code {
        "ThrottlingException" | "ThrottledException" => CloudError::Throttled {
            message: format!("{target} of {secret} was throttled: {message}"),
            retry_after: None,
        },
        "InternalServiceError" | "InternalServerError" | "InternalFailure" => {
            CloudError::Unavailable(format!("{target} of {secret} failed: {code}"))
        }
        _ if status >= 500 => {
            CloudError::Unavailable(format!("{target} of {secret} failed with status {status}"))
        }
        _ => CloudError::Rejected(format!("{target} of {secret} failed: {code}: {message}")),
    }
}
