//! Explicit AssumeRole credentials with bounded, non-redirecting STS transport.
use crate::Error;
use reqsign_aws_v4::{
    AssumeRoleCredentialProvider, AssumeRoleGrant, Credential, RequestSigner,
    StaticCredentialProvider,
};
use reqsign_core::{Context, HttpSend, ProvideCredential, ProvideCredentialChain, Signer};
use serde::Deserialize;
use std::{fmt, time::Duration};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub role_arn: String,
    pub region: String,
    pub session_name: String,
    pub external_id_env: Option<String>,
    #[serde(default = "duration")]
    pub duration_seconds: u32,
    #[cfg(test)]
    #[serde(skip)]
    pub(crate) test_endpoint: Option<String>,
}
fn duration() -> u32 {
    3600
}
impl Config {
    fn host(&self) -> Result<String, Error> {
        if self.region.is_empty()
            || self.region.len() > 64
            || !self
                .region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(Error::Config);
        }
        if self.region.starts_with("cn-") {
            Ok(format!("sts.{}.amazonaws.com.cn", self.region))
        } else if self.region.starts_with("us-gov-") || self.role_arn.starts_with("arn:aws:") {
            Ok(format!("sts.{}.amazonaws.com", self.region))
        } else {
            Err(Error::Config)
        }
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.host()?;
        if !(900..=43200).contains(&self.duration_seconds) {
            return Err(Error::Config);
        }
        let mut grant = AssumeRoleGrant::new(&self.role_arn, &self.session_name);
        if let Some(name) = &self.external_id_env {
            grant = grant.with_external_id(crate::storage::secret(name)?);
        }
        grant
            .validate_for_region(&self.region)
            .map_err(|_| Error::Config)
    }
    pub(crate) fn chain(
        &self,
        access: &str,
        secret: &str,
        token: Option<&str>,
    ) -> Result<ProvideCredentialChain<Credential>, Error> {
        let mut base = StaticCredentialProvider::new(access, secret);
        if let Some(token) = token {
            base = base.with_session_token(token);
        }
        self.chain_from_source(base)
    }
    pub(crate) fn chain_from_source(
        &self,
        base: impl ProvideCredential<Credential = Credential>,
    ) -> Result<ProvideCredentialChain<Credential>, Error> {
        self.validate()?;
        let transport = Transport::new(self.host()?)?;
        #[cfg(test)]
        let transport = Transport {
            test_endpoint: self.test_endpoint.clone(),
            ..transport
        };
        let context = Context::new().with_http_send(transport);
        let signer = Signer::new(
            Context::new(),
            base,
            RequestSigner::new("sts", &self.region),
        );
        let mut inner = AssumeRoleCredentialProvider::new(self.role_arn.clone(), signer)
            .with_region(self.region.clone())
            .with_regional_sts_endpoint()
            .with_role_session_name(self.session_name.clone())
            .with_duration_seconds(self.duration_seconds);
        if let Some(name) = &self.external_id_env {
            inner = inner.with_external_id(crate::storage::secret(name)?);
        }
        Ok(ProvideCredentialChain::new().push(Provider { context, inner }))
    }
}
struct Provider {
    context: Context,
    inner: AssumeRoleCredentialProvider,
}
impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SiriusAssumeRoleProvider")
    }
}
impl ProvideCredential for Provider {
    type Credential = Credential;
    async fn provide_credential(&self, _: &Context) -> reqsign_core::Result<Option<Credential>> {
        self.inner
            .provide_credential(&self.context)
            .await
            .map_err(|_| reqsign_core::Error::unexpected("STS credential acquisition failed"))
    }
}
#[derive(Clone)]
struct Transport {
    client: reqwest::Client,
    host: String,
    #[cfg(test)]
    test_endpoint: Option<String>,
}
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SiriusStsTransport")
    }
}
impl Transport {
    fn new(host: String) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            client,
            host,
            #[cfg(test)]
            test_endpoint: None,
        })
    }
}
impl HttpSend for Transport {
    async fn http_send(
        &self,
        request: http::Request<bytes::Bytes>,
    ) -> reqsign_core::Result<http::Response<bytes::Bytes>> {
        let error = || reqsign_core::Error::unexpected("STS transport failed");
        let (parts, body) = request.into_parts();
        if parts.uri.scheme_str() != Some("https")
            || parts.uri.authority().map(|a| a.as_str()) != Some(self.host.as_str())
            || !matches!(parts.method, http::Method::GET | http::Method::POST)
            || body.len() > 65536
        {
            return Err(error());
        }
        let target = parts.uri.to_string();
        #[cfg(test)]
        let target = match &self.test_endpoint {
            Some(endpoint) => format!(
                "{endpoint}{}",
                parts
                    .uri
                    .path_and_query()
                    .map(|v| v.as_str())
                    .unwrap_or("/")
            ),
            None => target,
        };
        let mut response = self
            .client
            .request(parts.method, target)
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|_| error())?;
        if response.status() != reqwest::StatusCode::OK
            || response.content_length().is_some_and(|n| n > 65536)
        {
            return Err(error());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| error())? {
            if bytes.len() + chunk.len() > 65536 {
                return Err(error());
            }
            bytes.extend_from_slice(&chunk);
        }
        http::Response::builder()
            .status(200)
            .body(bytes.into())
            .map_err(|_| error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    fn config(endpoint: String) -> Config {
        Config {
            role_arn: "arn:aws:iam::123456789012:role/fixture".into(),
            region: "us-east-1".into(),
            session_name: "sirius-fixture".into(),
            external_id_env: None,
            duration_seconds: 3600,
            test_endpoint: Some(endpoint),
        }
    }
    pub(crate) fn response(key: &str, seconds: i64) -> String {
        let expiration = (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339();
        format!("<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{key}</AccessKeyId><SecretAccessKey>synthetic-role-secret</SecretAccessKey><SessionToken>synthetic-session-token</SessionToken><Expiration>{expiration}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>")
    }
    async fn server(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        (
            origin,
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        )
    }
    #[tokio::test]
    async fn sts_refreshes_near_expiry_and_coalesces_concurrent_signing() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().fallback({
            let hits = hits.clone();
            move |headers: axum::http::HeaderMap, uri: axum::http::Uri| {
                let hits = hits.clone();
                async move {
                    assert!(headers["authorization"]
                        .to_str()
                        .unwrap()
                        .contains("Credential=base-access/"));
                    assert!(headers["authorization"]
                        .to_str()
                        .unwrap()
                        .contains("/us-east-1/sts/aws4_request"));
                    assert!(uri.query().unwrap().contains("Action=AssumeRole"));
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    response(
                        if n == 0 {
                            "ASIAFIRST123456789"
                        } else {
                            "ASIASECOND12345678"
                        },
                        if n == 0 { 30 } else { 3600 },
                    )
                }
            }
        });
        let (origin, task) = server(app).await;
        let chain = config(origin)
            .chain("base-access", "base-secret", None)
            .unwrap();
        let signer = Arc::new(Signer::new(
            Context::new(),
            chain,
            RequestSigner::new("s3", "us-east-1"),
        ));
        let mut first = http::Request::get("https://bucket.example/object")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        signer.sign(&mut first, None).await.unwrap();
        assert!(first.headers["authorization"]
            .to_str()
            .unwrap()
            .contains("Credential=ASIAFIRST123456789/"));
        let tasks = (0..16)
            .map(|_| {
                let signer = signer.clone();
                tokio::spawn(async move {
                    let mut request = http::Request::get("https://bucket.example/object")
                        .body(())
                        .unwrap()
                        .into_parts()
                        .0;
                    signer.sign(&mut request, None).await.unwrap();
                    assert!(request.headers["authorization"]
                        .to_str()
                        .unwrap()
                        .contains("Credential=ASIASECOND12345678/"));
                    assert_eq!(
                        request.headers["x-amz-security-token"],
                        "synthetic-session-token"
                    );
                })
            })
            .collect::<Vec<_>>();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        task.abort();
    }
    #[tokio::test]
    async fn sts_refuses_redirect_oversize_malformed_expired_and_denied_responses_without_secret_errors(
    ) {
        for mode in 0..5 {
            let app = axum::Router::new().fallback(move || async move {
                let (status, body) = match mode {
                    0 => (302, "secret-redirect".into()),
                    1 => (200, "X".repeat(65537)),
                    2 => (200, "malformed-secret".into()),
                    3 => (200, response("ASIAEXPIRED1234567", -60)),
                    _ => (403, "denied-secret".into()),
                };
                axum::http::Response::builder()
                    .status(status)
                    .header("location", "http://127.0.0.1:1/must-not-follow")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            });
            let (origin, task) = server(app).await;
            let chain = config(origin)
                .chain("base-access", "base-secret", None)
                .unwrap();
            let signer = Signer::new(Context::new(), chain, RequestSigner::new("s3", "us-east-1"));
            let mut request = http::Request::get("https://bucket.example/object")
                .body(())
                .unwrap()
                .into_parts()
                .0;
            let error = signer
                .sign(&mut request, None)
                .await
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret") && !error.contains("127.0.0.1"));
            task.abort();
        }
    }
    #[test]
    fn sts_configuration_validates_role_partition_session_and_duration() {
        let mut c = config(String::new());
        c.validate().unwrap();
        c.duration_seconds = 899;
        assert!(c.validate().is_err());
        c.duration_seconds = 43201;
        assert!(c.validate().is_err());
        c.duration_seconds = 3600;
        c.session_name = "bad/name".into();
        assert!(c.validate().is_err());
        c.session_name = "fixture".into();
        c.role_arn = "arn:aws-cn:iam::123456789012:role/fixture".into();
        assert!(c.validate().is_err());
        c.region = "cn-north-1".into();
        c.validate().unwrap();
        c.region = "us-east-1.evil.test".into();
        assert!(c.validate().is_err());
    }
}
