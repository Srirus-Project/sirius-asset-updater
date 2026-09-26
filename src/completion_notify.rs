//! Bounded completion delivery; the job ledger owns durability and acknowledgement.
use crate::{
    jobs::{Completion, CompletionTarget},
    region::Region,
    Error,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub region: Region,
    pub endpoint: String,
    pub token_env: String,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "retry")]
    pub retry_seconds: u64,
}
fn timeout() -> u64 {
    10
}
fn retry() -> u64 {
    30
}

pub(crate) struct Target {
    pub identity: String,
    pub name: String,
    pub region: Region,
    endpoint: reqwest::Url,
    token: reqwest::header::HeaderValue,
    client: reqwest::Client,
    pub retry_seconds: u64,
}
#[derive(Clone, Default, Serialize)]
pub(crate) struct DeliveryState {
    pub attempts: u64,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_acknowledged_at: Option<DateTime<Utc>>,
    pub last_error: Option<&'static str>,
    #[serde(skip)]
    pub due: Option<Instant>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    schema_version: u8,
    job_id: String,
}

pub(crate) fn load(configs: &[Config], forbidden: &[String]) -> Result<Vec<Target>, Error> {
    if configs.len() > 16 {
        return Err(Error::Config);
    }
    let mut names = HashSet::new();
    let mut tokens = HashSet::new();
    configs
        .iter()
        .map(|c| {
            let url = reqwest::Url::parse(&c.endpoint).map_err(|_| Error::Config)?;
            let loopback = url.host_str().is_some_and(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            });
            if c.name.is_empty()
                || c.name.len() > 64
                || !c
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                || !names.insert(c.name.clone())
                || c.region == Region::Cn
                || !(1..=60).contains(&c.timeout_seconds)
                || !(1..=3600).contains(&c.retry_seconds)
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            {
                return Err(Error::Config);
            }
            let secret = std::env::var(&c.token_env).map_err(|_| Error::Secret)?;
            if secret.is_empty()
                || secret.len() > 4096
                || !secret.bytes().all(|b| b.is_ascii_graphic())
                || forbidden.contains(&secret)
                || !tokens.insert(secret.clone())
            {
                return Err(Error::Config);
            }
            let mut token = reqwest::header::HeaderValue::from_str(&format!("Bearer {secret}"))
                .map_err(|_| Error::Config)?;
            token.set_sensitive(true);
            let identity = hex::encode(Sha256::digest(
                // Hash input is frozen (hk keeps its pre-1.2.1 tag) so journaled pending
                // deliveries keep matching their target after the region rename.
                sonic_rs::to_vec(&(
                    c.name.as_str(),
                    c.region.persisted_digest_tag(),
                    url.as_str(),
                ))
                .map_err(|_| Error::Config)?,
            ));
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(c.timeout_seconds.min(10)))
                .timeout(Duration::from_secs(c.timeout_seconds))
                .build()
                .map_err(|_| Error::Config)?;
            Ok(Target {
                identity,
                name: c.name.clone(),
                region: c.region,
                endpoint: url,
                token,
                client,
                retry_seconds: c.retry_seconds,
            })
        })
        .collect()
}
impl Target {
    pub fn ledger_target(&self) -> CompletionTarget {
        CompletionTarget {
            identity: self.identity.clone(),
            region: self.region,
        }
    }
    pub async fn deliver(&self, event: &Completion) -> Result<(), &'static str> {
        if event.request.region != self.region {
            return Err("region_mismatch");
        }
        let body = sonic_rs::to_vec(event).map_err(|_| "invalid_event")?;
        if body.len() > 65536 {
            return Err("invalid_event");
        }
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, self.token.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header("idempotency-key", &event.job_id)
            .body(body)
            .send()
            .await
            .map_err(|_| "transport_failed")?;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            return Err("receiver_rejected");
        }
        if response.content_length().is_some_and(|n| n > 1024) {
            return Err("invalid_acknowledgement");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "transport_failed")? {
            if bytes.len() + chunk.len() > 1024 {
                return Err("invalid_acknowledgement");
            }
            bytes.extend_from_slice(&chunk);
        }
        let ack: Ack = sonic_rs::from_slice(&bytes).map_err(|_| "invalid_acknowledgement")?;
        if ack.schema_version != 1 || ack.job_id != event.job_id {
            return Err("invalid_acknowledgement");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Response, StatusCode},
        routing::post,
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    fn config(endpoint: String) -> Config {
        let token_env = format!("SIRIUS_NOTICE_TEST_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&token_env, "synthetic-notice-token");
        Config {
            name: "receiver".into(),
            region: Region::Jp,
            endpoint,
            token_env,
            timeout_seconds: 1,
            retry_seconds: 1,
        }
    }
    fn event() -> Completion {
        sonic_rs::from_value(&sonic_rs::json!({
            "schema_version":1, "job_id":uuid::Uuid::new_v4().to_string(),
            "request":{"region":"jp","profile":"verify","operation":"verify"},
            "completed_at":Utc::now(), "outcome":{"publication_id":null,"export":null,
            "verification":{"full_catalog":false,"catalog_remote_files":0,"catalog_verified":true,
            "region":"jp","platform":"iOS","environment":"production","resource_version":"synthetic",
            "platform_hash":"synthetic","catalog_sha256":"a".repeat(64),"asset_files_verified":0,
            "asset_bytes_verified":0,"planned_remote_files":0,"embedded_locations":0,"decrypted_bundles":0}}
        })).unwrap()
    }
    #[test]
    fn validates_scope_and_keeps_identity_across_token_rotation() {
        let c = config("https://receiver.example/completions".into());
        let first = load(std::slice::from_ref(&c), &[]).unwrap()[0]
            .identity
            .clone();
        assert!(load(std::slice::from_ref(&c), &["synthetic-notice-token".into()]).is_err());
        assert!(load(&[c.clone(), c.clone()], &[]).is_err());
        std::env::set_var(&c.token_env, "rotated-notice-token");
        assert_eq!(
            load(std::slice::from_ref(&c), &[]).unwrap()[0].identity,
            first
        );
        // hk targets keep the digest they had under the pre-1.2.1 region name.
        let mut hk = c.clone();
        hk.region = Region::Hk;
        let url = hk.endpoint.clone();
        assert_eq!(
            load(std::slice::from_ref(&hk), &[]).unwrap()[0].identity,
            hex::encode(Sha256::digest(
                sonic_rs::to_vec(&(hk.name.as_str(), "tw", url.as_str())).unwrap()
            ))
        );
        let mut changed = c.clone();
        changed.endpoint = "https://different.example/completions".into();
        assert_ne!(load(&[changed], &[]).unwrap()[0].identity, first);
        for endpoint in [
            "http://receiver.example/completions",
            "https://user:secret@example.com/x",
            "https://example.com/x?secret=x",
            "https://example.com/x#fragment",
        ] {
            let mut invalid = c.clone();
            invalid.endpoint = endpoint.into();
            assert!(load(&[invalid], &[]).is_err());
        }
        let mut cn = c.clone();
        cn.region = Region::Cn;
        assert!(load(&[cn], &[]).is_err());
        let mut same_token = c.clone();
        same_token.name = "another".into();
        assert!(load(&[c, same_token], &[]).is_err());
    }
    #[tokio::test]
    async fn rejects_redirect_malformed_oversized_ack_and_stalled_body() {
        let destination_hits = Arc::new(AtomicUsize::new(0));
        let hits = destination_hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let destination = format!("{root}/destination");
        let router = Router::new()
            .route(
                "/redirect",
                post(move || {
                    let destination = destination.clone();
                    async move {
                        Response::builder()
                            .status(302)
                            .header("location", destination)
                            .body(Body::empty())
                            .unwrap()
                    }
                }),
            )
            .route(
                "/destination",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        StatusCode::ACCEPTED
                    }
                }),
            )
            .route(
                "/malformed",
                post(|| async { (StatusCode::ACCEPTED, "not json") }),
            )
            .route(
                "/oversized",
                post(|| async { (StatusCode::ACCEPTED, "x".repeat(1025)) }),
            )
            .route(
                "/wrong-id",
                post(|| async {
                    (
                        StatusCode::ACCEPTED,
                        r#"{"schema_version":1,"job_id":"wrong"}"#,
                    )
                }),
            )
            .route(
                "/stall",
                post(|| async {
                    Response::builder()
                        .status(202)
                        .body(Body::from_stream(futures_util::stream::pending::<
                            Result<bytes::Bytes, std::io::Error>,
                        >()))
                        .unwrap()
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        for (path, expected) in [
            ("redirect", "receiver_rejected"),
            ("malformed", "invalid_acknowledgement"),
            ("oversized", "invalid_acknowledgement"),
            ("wrong-id", "invalid_acknowledgement"),
            ("stall", "transport_failed"),
        ] {
            let target = load(&[config(format!("{root}/{path}"))], &[])
                .unwrap()
                .remove(0);
            let result = tokio::time::timeout(Duration::from_secs(3), target.deliver(&event()))
                .await
                .unwrap();
            assert_eq!(result, Err(expected));
        }
        assert_eq!(destination_hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
