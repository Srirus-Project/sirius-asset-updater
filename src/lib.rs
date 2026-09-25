//! Sirius snapshot consumption and bounded asset acquisition.
pub mod assets;
mod cache;
pub mod catalog;
pub mod export;
pub mod export_verify;
pub mod media_backend;
#[cfg(feature = "media-ffi")]
pub mod media_ffi;
pub mod network;
pub mod proxy;
pub mod raw_bundles;
pub mod readiness;
pub mod storage;
mod update;
pub mod verify;
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage publication failed")]
    Storage,
    #[error("unknown or empty catalog selection key")]
    Selection,
    #[error("cn is reserved; no verified endpoint or protocol is available")]
    ReservedRegion,
    #[error("export failed: {0}")]
    Export(String),
    #[error("published output failed offline integrity verification")]
    Verification,
    #[error("another updater is using this cache directory")]
    Busy,
    #[error("preflight failed; run the check command to inspect configuration fields")]
    Preflight,
    #[error("game service is unavailable or under maintenance")]
    Unavailable,
    #[error("job execution deadline exceeded")]
    JobTimeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("unsupported resource provider")]
    Provider,
    #[error("unsafe or unrecognized resource path")]
    AssetPath,
    #[error("invalid Unity bundle or incorrect decryption configuration")]
    Bundle,
    #[error("invalid configuration")]
    Config,
    #[error("required secret is unavailable")]
    Secret,
    #[error("snapshot is stale or incompatible")]
    Snapshot,
    #[error("request failed")]
    Transport,
    #[error("upstream HTTP status {0}")]
    Status(u16),
    #[error("response exceeds configured size limit")]
    Size,
    #[error("unsupported catalog format (expected Addressables binary v2)")]
    Catalog,
    #[error("local file operation failed")]
    Io,
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Storage => "storage_failed",
            Self::Selection => "invalid_selection",
            Self::ReservedRegion => "reserved_region",
            Self::Export(_) => "export_failed",
            Self::Verification => "verification_failed",
            Self::Busy => "cache_busy",
            Self::Preflight => "preflight_failed",
            Self::Unavailable => "upstream_unavailable",
            Self::JobTimeout => "job_timeout",
            Self::Cancelled => "cancelled",
            Self::Provider => "unsupported_provider",
            Self::AssetPath => "invalid_asset_path",
            Self::Bundle => "invalid_bundle",
            Self::Config => "invalid_config",
            Self::Secret => "secret_unavailable",
            Self::Snapshot => "invalid_snapshot",
            Self::Transport => "transport_failed",
            Self::Status(_) => "upstream_http_status",
            Self::Size => "size_limit",
            Self::Catalog => "invalid_catalog",
            Self::Io => "io_failed",
        }
    }
    pub fn http_status(&self) -> Option<u16> {
        if let Self::Status(code) = self {
            Some(*code)
        } else {
            None
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    #[serde(default)]
    pub region: region::Region,
    #[serde(default)]
    pub platform: Option<region::Platform>,
    #[serde(default)]
    pub protocol_version: Option<String>,
    pub game_api_root: String,
    #[serde(default)]
    pub network: network::Network,
    /// Use /api/v1/{region} and /internal/v1/{region} on a multi-region proxy.
    #[serde(default)]
    pub regional_routes: bool,
    pub internal_token_env: String,
    #[serde(default)]
    pub refresh_token_env: Option<String>,
    pub environment: String,
    pub client_version: String,
    pub output: PathBuf,
    pub cdn_roots: BTreeMap<String, CdnAuth>,
    #[serde(default)]
    pub assets: Option<assets::AssetConfig>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnAuth {
    pub username_env: String,
    pub credential_env: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<region::Region>,
    pub schema_version: u8,
    pub environment: String,
    pub platform: String,
    pub client_version: String,
    pub protocol_version: String,
    pub master_version: Option<String>,
    pub resource_version: String,
    pub platform_hash: String,
    pub effective_cdn_root: String,
    pub credential_ref: String,
    pub observed_at: DateTime<Utc>,
    pub source: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResponse {
    pub snapshot: Snapshot,
    pub stale: bool,
}
#[derive(Serialize, Deserialize)]
pub struct Receipt {
    pub snapshot: Snapshot,
    pub catalog_url: String,
    pub bytes: u64,
    pub sha256: String,
    pub downloaded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<update::UpdateReceipt>,
}

fn component(s: &str) -> bool {
    !s.is_empty()
        && !matches!(s, "." | "..")
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}
fn root(s: &str, allow_local: bool) -> bool {
    Url::parse(s).is_ok_and(|u| {
        u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
            && !s.ends_with('/')
            && (u.scheme() == "https"
                || (allow_local
                    && u.scheme() == "http"
                    && matches!(u.host_str(), Some("127.0.0.1" | "[::1]"))))
    })
}
fn secret(name: &str) -> Result<String, Error> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or(Error::Secret)
}
fn cdn_root(s: &str) -> bool {
    Url::parse(s).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && !s.ends_with('/')
            && !s.contains('%')
            && !s.contains('\\')
            && !s.split('/').any(|p| matches!(p, "." | ".."))
            && u.path().split('/').all(|p| {
                p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
    })
}
impl Config {
    pub(crate) fn api_url(&self, internal: bool, suffix: &str) -> String {
        let scope = if internal { "internal" } else { "api" };
        if self.regional_routes {
            format!(
                "{}/{scope}/v1/{}/{suffix}",
                self.game_api_root,
                self.region.name()
            )
        } else {
            format!("{}/{scope}/v1/{suffix}", self.game_api_root)
        }
    }

    pub fn platform(&self) -> region::Platform {
        self.platform.unwrap_or(self.region.default_platform())
    }
    pub fn protocol_version(&self) -> &str {
        self.protocol_version
            .as_deref()
            .unwrap_or(self.region.protocol_version())
    }

    pub fn validate(&self) -> Result<(), Error> {
        if let Some(log) = &self.logging {
            log.validate().map_err(|_| Error::Config)?;
        }
        self.network.validate()?;
        if self.region == region::Region::Cn {
            return Err(Error::ReservedRegion);
        }
        for value in self.cdn_roots.keys() {
            if let Ok(url) = Url::parse(value) {
                if !self
                    .region
                    .matches_known_service(url.host_str().unwrap_or(""), url.path())
                {
                    return Err(Error::Config);
                }
            }
        }
        if let Some(assets) = &self.assets {
            assets.validate()?;
        }
        if !root(&self.game_api_root, true)
            || !component(&self.environment)
            || !component(&self.client_version)
            || !component(self.protocol_version())
            || self.output.as_os_str().is_empty()
            || self.cdn_roots.is_empty()
            || self.cdn_roots.keys().any(|s| !cdn_root(s))
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}
impl Snapshot {
    pub fn region_identity(&self) -> Result<region::Region, Error> {
        match (self.schema_version, self.region) {
            (1, None) if self.platform == "iOS" && self.protocol_version == "1.0.3" => {
                Ok(region::Region::Jp)
            }
            (2, Some(region))
                if region != region::Region::Cn
                    && matches!(self.platform.as_str(), "iOS" | "Android") =>
            {
                Ok(region)
            }
            _ => Err(Error::Verification),
        }
    }
}
impl SnapshotResponse {
    pub fn catalog_url(&self, config: &Config, now: DateTime<Utc>) -> Result<String, Error> {
        let s = &self.snapshot;
        let age = now.signed_duration_since(s.observed_at).num_milliseconds();
        if self.stale
            || !(0..=300_000).contains(&age)
            || !match s.schema_version {
                1 => {
                    s.region.is_none()
                        && config.region == region::Region::Jp
                        && config.platform() == region::Platform::Ios
                        && s.protocol_version == "1.0.3"
                }
                2 => s.region == Some(config.region),
                _ => false,
            }
            || s.environment != config.environment
            || s.platform != config.platform().name()
            || s.client_version != config.client_version
            || s.protocol_version != config.protocol_version()
            || s.source != "remote"
            || !component(&s.resource_version)
            || !component(&s.platform_hash)
        {
            return Err(Error::Snapshot);
        }
        let auth = config
            .cdn_roots
            .get(&s.effective_cdn_root)
            .ok_or(Error::Snapshot)?;
        // A remote reference may not select an arbitrary local environment variable.
        if s.credential_ref != auth.credential_env {
            return Err(Error::Snapshot);
        }
        Ok(format!(
            "{}/asset/{}/{}/{}/catalog_main.bin",
            s.effective_cdn_root,
            s.resource_version,
            config.platform().name(),
            s.platform_hash
        ))
    }
}
pub struct CatalogClient {
    config: Config,
    http: Client,
    cdn_http: Client,
    service_download_gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
}
impl CatalogClient {
    pub fn new(config: Config) -> Result<Self, Error> {
        config.validate()?;
        Self::build(config)
    }
    fn build(config: Config) -> Result<Self, Error> {
        let http = proxy::builder(&config.network, config.network.api_proxy.as_ref())?
            .build()
            .map_err(|_| Error::Transport)?;
        let cdn_http = proxy::cdn_builder(&config.network)?
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            config,
            http,
            cdn_http,
            service_download_gate: None,
        })
    }

    pub(crate) fn set_service_download_gate(
        &mut self,
        gate: std::sync::Arc<tokio::sync::Semaphore>,
    ) {
        self.service_download_gate = Some(gate);
    }
    async fn cdn_attempt<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        tokio::time::timeout(
            std::time::Duration::from_millis(self.config.network.download_timeout_ms),
            async {
                let _permit = match &self.service_download_gate {
                    Some(gate) => Some(
                        gate.clone()
                            .acquire_owned()
                            .await
                            .map_err(|_| Error::Cancelled)?,
                    ),
                    None => None,
                };
                operation.await
            },
        )
        .await
        .map_err(|_| Error::Transport)?
    }
    pub async fn fetch(&self) -> Result<PathBuf, Error> {
        if !self.config.check_secrets().ready {
            return Err(Error::Preflight);
        }
        tracing::info!(
            stage = "output_preflight",
            region = self.config.region.name(),
            "Download stage"
        );
        tokio::fs::create_dir_all(&self.config.output)
            .await
            .map_err(|_| Error::Io)?;
        tempfile::Builder::new()
            .prefix(".preflight-")
            .tempdir_in(&self.config.output)
            .map_err(|_| Error::Io)?
            .close()
            .map_err(|_| Error::Io)?;
        let _cache_guard = self
            .config
            .assets
            .as_ref()
            .and_then(|c| c.cache_directory.as_ref())
            .map(|root| cache::Guard::acquire_download(root))
            .transpose()?;
        tracing::info!(
            stage = "snapshot",
            region = self.config.region.name(),
            "Download stage"
        );
        self.download(self.observed_snapshot().await?).await
    }
    async fn read_snapshot(&self) -> Result<SnapshotResponse, Error> {
        let token = secret(&self.config.internal_token_env)?;
        let url = self.config.api_url(true, "resources/snapshot");
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .timeout(Duration::from_millis(
                self.config.network.snapshot_timeout_ms,
            ))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let bytes = readiness::bounded_api_response(response).await?;
        let snapshot: SnapshotResponse =
            sonic_rs::from_slice(&bytes).map_err(|_| Error::Snapshot)?;
        Ok(snapshot)
    }
    async fn download_catalog_once(
        &self,
        url: &str,
        username: &str,
        password: &str,
        path: &Path,
    ) -> Result<(u64, String), Error> {
        self.cdn_attempt(self.download_catalog_inner(url, username, password, path))
            .await
    }
    async fn download_catalog_inner(
        &self,
        url: &str,
        username: &str,
        password: &str,
        path: &Path,
    ) -> Result<(u64, String), Error> {
        let mut response = self
            .cdn_http
            .get(url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        status(&response)?;
        const MAX: u64 = 64 * 1024 * 1024;
        if response.content_length().is_some_and(|s| s > MAX) {
            return Err(Error::Size);
        }
        let mut file = tokio::fs::File::create(path).await.map_err(|_| Error::Io)?;
        let mut size = 0u64;
        let mut digest = Sha256::new();
        let mut prefix = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            size += chunk.len() as u64;
            if size > MAX {
                return Err(Error::Size);
            }
            let take = (8 - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(|_| Error::Io)?;
        }
        if prefix != [0x42, 0x89, 0xe3, 0x0d, 2, 0, 0, 0] || size <= 8 {
            return Err(Error::Catalog);
        }
        file.sync_all().await.map_err(|_| Error::Io)?;
        drop(file);
        Ok((size, hex::encode(digest.finalize())))
    }
    async fn download(&self, snapshot: SnapshotResponse) -> Result<PathBuf, Error> {
        let url = snapshot.catalog_url(&self.config, Utc::now())?;
        let auth = self
            .config
            .cdn_roots
            .get(&snapshot.snapshot.effective_cdn_root)
            .ok_or(Error::Snapshot)?;
        let username = secret(&auth.username_env)?;
        let password = secret(&auth.credential_env)?;
        tracing::info!(
            stage = "catalog_download",
            region = self.config.region.name(),
            "Download stage"
        );
        tokio::fs::create_dir_all(&self.config.output)
            .await
            .map_err(|_| Error::Io)?;
        // Private staging directory is removed on error/cancellation; neither catalog nor receipt
        // becomes visible as a finished run until the directory rename succeeds.
        let staging = tempfile::Builder::new()
            .prefix(".catalog-")
            .tempdir_in(&self.config.output)
            .map_err(|_| Error::Io)?;
        let mut downloaded = None;
        for attempt in 0..self.config.network.catalog_retry.attempts {
            match self
                .download_catalog_once(
                    &url,
                    &username,
                    &password,
                    &staging.path().join("catalog_main.bin"),
                )
                .await
            {
                Ok(result) => {
                    downloaded = Some(result);
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        stage = "catalog_attempt_failed",
                        attempt = attempt + 1,
                        error_code = error.code(),
                        status = error.http_status(),
                        "Catalog request failed"
                    );
                    if !self.config.network.catalog_retry.retry(&error, attempt) {
                        return Err(error);
                    }
                    tokio::time::sleep(self.config.network.catalog_retry.delay(attempt)).await;
                }
            }
        }
        let (size, sha256) = downloaded.ok_or(Error::Transport)?;
        // Catalog-only launch probes must validate structure, not just the magic.
        if self.config.assets.is_none() {
            let data = tokio::fs::read(staging.path().join("catalog_main.bin"))
                .await
                .map_err(|_| Error::Io)?;
            catalog::Catalog::parse(&data)?;
        }
        let update = if let Some(config) = &self.config.assets {
            Some(
                self.download_assets(&snapshot, staging.path(), config)
                    .await?,
            )
        } else {
            if self.config.refresh_token_env.is_some() {
                self.revalidate(&snapshot).await?;
            } else {
                snapshot.catalog_url(&self.config, Utc::now())?;
            }
            None
        };
        let receipt = Receipt {
            snapshot: snapshot.snapshot,
            catalog_url: url,
            bytes: size,
            sha256,
            downloaded_at: Utc::now(),
            update,
        };
        let receipt_bytes = sonic_rs::to_vec_pretty(&receipt).map_err(|_| Error::Io)?;
        write_receipt(staging.path(), &receipt_bytes).await?;
        let path = self.config.output.join(format!(
            "catalog-{}-{}-{}",
            self.config.region.name(),
            self.config.platform().name(),
            uuid::Uuid::new_v4()
        ));
        tracing::info!(
            stage = "publish",
            region = self.config.region.name(),
            "Download stage"
        );
        tokio::fs::rename(staging.path(), &path)
            .await
            .map_err(|_| Error::Io)?;
        Ok(path)
    }
}
fn status(response: &reqwest::Response) -> Result<(), Error> {
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(Error::Status(response.status().as_u16()))
    }
}
async fn write_receipt(dir: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = tokio::fs::File::create(dir.join("receipt.json"))
        .await
        .map_err(|_| Error::Io)?;
    file.write_all(bytes).await.map_err(|_| Error::Io)?;
    file.sync_all().await.map_err(|_| Error::Io)
}

#[cfg(test)]
mod tests;

pub mod region;

pub mod jobs;

pub mod service;

pub mod export_options;

pub mod server;

pub mod access_log;

pub mod application_log;

mod media_gate;

mod resource_budget;

pub mod cpu_policy;

pub mod stage_limits;

pub mod cpu_throttle;

pub mod storage_sts;

pub mod storage_credentials;

pub mod completion_notify;

pub mod read_policy;
