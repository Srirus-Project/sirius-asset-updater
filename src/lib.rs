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
    #[error("remote configuration source is unavailable")]
    RemoteConfig,
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
            Self::RemoteConfig => "remote_config_unavailable",
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
    /// Global only: download the localized catalog `catalog_{version}_{locale}.bin` instead of
    /// the base (Japanese) catalog. One of [`CATALOG_LOCALES`]; omitted selects the base catalog.
    #[serde(default)]
    pub catalog_locale: Option<String>,
}
/// Localized Global catalog suffixes (the client's `LocaleManager` codes; Japanese is the base).
pub const CATALOG_LOCALES: &[&str] = &["en", "zh-Hant", "zh-Hans", "ko"];
/// The only absolute remote-bundle host a Global catalog may name. The client replaces it with
/// its CDN root; any other absolute URL is rejected.
pub const GLOBAL_REMOTE_PLACEHOLDER: &str = "https://dummy.net";
/// CDN authorization for one configured root.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CdnAuthorization {
    /// HTTP Basic with `username_env`/`credential_env` (required for JP).
    #[default]
    Basic,
    /// No Authorization header; accepted only for HK/EN/KR roots without credential references.
    None,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnAuth {
    #[serde(default)]
    pub authorization: CdnAuthorization,
    /// Required for `basic`; must be omitted for `none`.
    #[serde(default)]
    pub username_env: String,
    /// Required for `basic`; must be omitted for `none`.
    #[serde(default)]
    pub credential_env: String,
}
/// How a client family lays out catalogs and bundles on its CDN.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogLayout {
    /// `{root}/asset/{version}/{platform}/{hash}/catalog_main.bin`, bundles beside it.
    Jp,
    /// `{root}/asset/{platform}/catalog_{version}[_{locale}].bin` with a sibling `.hash`;
    /// bundles in `{root}/asset/{platform}`.
    Global,
}
impl CatalogLayout {
    fn for_region(region: region::Region) -> Self {
        if region.family() == "global" {
            Self::Global
        } else {
            Self::Jp
        }
    }
}
/// Everything a download needs from a validated snapshot. URLs are derived from the layout,
/// never from string surgery on another URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTarget {
    pub layout: CatalogLayout,
    pub catalog_url: String,
    /// Global: the catalog's `.hash` URL, read before and after the download.
    pub hash_url: Option<String>,
    pub bundle_base_url: String,
    /// Global: `https://dummy.net/asset/{platform}`, mapped onto `bundle_base_url`.
    pub remote_placeholder: Option<String>,
    pub authorization: CdnAuthorization,
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
    /// Schema 3 only (all four fields): explicit layout, base catalog URL, bundle directory
    /// and CDN authorization. They must equal what the layout derives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_layout: Option<CatalogLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdn_authorization: Option<CdnAuthorization>,
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
    /// Since 1.2.1. Absent in older receipts, which are JP layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_layout: Option<CatalogLayout>,
    /// Since 1.2.1: the directory remote bundle paths were resolved against. Older receipts
    /// derive it from a JP `catalog_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_base_url: Option<String>,
    /// Global: the configured localized catalog, absent for the base catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_locale: Option<String>,
    /// Global: the catalog `.hash` read before and after the download.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_hash: Option<String>,
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
        let global = self.region.family() == "global";
        if self
            .catalog_locale
            .as_deref()
            .is_some_and(|locale| !global || !CATALOG_LOCALES.contains(&locale))
        {
            return Err(Error::Config);
        }
        for auth in self.cdn_roots.values() {
            let valid = match auth.authorization {
                CdnAuthorization::Basic => {
                    !auth.username_env.is_empty() && !auth.credential_env.is_empty()
                }
                // JP CDNs require Basic; anonymous access is verified only for Global.
                CdnAuthorization::None => {
                    global && auth.username_env.is_empty() && auth.credential_env.is_empty()
                }
            };
            if !valid {
                return Err(Error::Config);
            }
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
                    && self.catalog_layout.is_none()
                    && matches!(self.platform.as_str(), "iOS" | "Android") =>
            {
                Ok(region)
            }
            (3, Some(region))
                if region != region::Region::Cn
                    && self.catalog_layout == Some(CatalogLayout::for_region(region))
                    && matches!(self.platform.as_str(), "iOS" | "Android") =>
            {
                Ok(region)
            }
            _ => Err(Error::Verification),
        }
    }
}
pub(crate) fn catalog_hash(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}
fn lower_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl SnapshotResponse {
    pub fn catalog_url(&self, config: &Config, now: DateTime<Utc>) -> Result<String, Error> {
        Ok(self.target(config, now)?.catalog_url)
    }
    /// Validates identity, freshness, layout and credential scope before any CDN request.
    pub fn target(&self, config: &Config, now: DateTime<Utc>) -> Result<CatalogTarget, Error> {
        let s = &self.snapshot;
        let age = now.signed_duration_since(s.observed_at).num_milliseconds();
        let explicit = [
            s.catalog_layout.is_some(),
            s.catalog_url.is_some(),
            s.bundle_base_url.is_some(),
            s.cdn_authorization.is_some(),
        ];
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
                3 => s.region == Some(config.region),
                _ => false,
            }
            // Schema 3 states the layout explicitly; earlier schemas never carry it.
            || explicit
                .iter()
                .any(|present| *present != (s.schema_version == 3))
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
        let layout = s.catalog_layout.unwrap_or(CatalogLayout::Jp);
        let authorization = s.cdn_authorization.unwrap_or_default();
        if layout != CatalogLayout::for_region(config.region)
            || (layout == CatalogLayout::Global && !lower_hex32(&s.platform_hash))
            || (layout == CatalogLayout::Jp && config.catalog_locale.is_some())
        {
            return Err(Error::Snapshot);
        }
        let auth = config
            .cdn_roots
            .get(&s.effective_cdn_root)
            .ok_or(Error::Snapshot)?;
        // A remote reference may not select an arbitrary local environment variable, and the
        // snapshot cannot turn a Basic root anonymous (or the reverse).
        let expected_ref = match auth.authorization {
            CdnAuthorization::Basic => auth.credential_env.as_str(),
            CdnAuthorization::None => "",
        };
        if authorization != auth.authorization || s.credential_ref != expected_ref {
            return Err(Error::Snapshot);
        }
        let root = &s.effective_cdn_root;
        let platform = config.platform().name();
        let version = &s.resource_version;
        let (base_catalog, target) = match layout {
            CatalogLayout::Jp => {
                let base = format!("{root}/asset/{version}/{platform}/{}", s.platform_hash);
                let catalog = format!("{base}/catalog_main.bin");
                (
                    catalog.clone(),
                    CatalogTarget {
                        layout,
                        catalog_url: catalog,
                        hash_url: None,
                        bundle_base_url: base,
                        remote_placeholder: None,
                        authorization,
                    },
                )
            }
            CatalogLayout::Global => {
                let base = format!("{root}/asset/{platform}");
                let stem = match &config.catalog_locale {
                    Some(locale) => format!("{base}/catalog_{version}_{locale}"),
                    None => format!("{base}/catalog_{version}"),
                };
                (
                    format!("{base}/catalog_{version}.bin"),
                    CatalogTarget {
                        layout,
                        catalog_url: format!("{stem}.bin"),
                        hash_url: Some(format!("{stem}.hash")),
                        bundle_base_url: base,
                        remote_placeholder: Some(format!(
                            "{GLOBAL_REMOTE_PLACEHOLDER}/asset/{platform}"
                        )),
                        authorization,
                    },
                )
            }
        };
        if s.catalog_url
            .as_ref()
            .is_some_and(|url| *url != base_catalog)
            || s.bundle_base_url
                .as_ref()
                .is_some_and(|url| *url != target.bundle_base_url)
        {
            return Err(Error::Snapshot);
        }
        Ok(target)
    }
}
impl Receipt {
    /// Bundle directory and optional Global placeholder for offline planning. Older receipts
    /// (JP) derive the directory from their catalog URL; newer ones must be consistent with it.
    pub fn remote(&self) -> Result<(String, Option<String>), Error> {
        let layout = self.catalog_layout.unwrap_or(CatalogLayout::Jp);
        let platform = self.snapshot.platform.as_str();
        if !matches!(platform, "iOS" | "Android") {
            return Err(Error::Verification);
        }
        match layout {
            CatalogLayout::Jp => {
                let derived = self
                    .catalog_url
                    .strip_suffix("/catalog_main.bin")
                    .ok_or(Error::Verification)?;
                if self
                    .bundle_base_url
                    .as_ref()
                    .is_some_and(|base| base != derived)
                    || self.catalog_locale.is_some()
                {
                    return Err(Error::Verification);
                }
                Ok((derived.to_owned(), None))
            }
            CatalogLayout::Global => {
                let base = self.bundle_base_url.as_ref().ok_or(Error::Verification)?;
                let expected = format!("{}/asset/{platform}", self.snapshot.effective_cdn_root);
                let stem = match &self.catalog_locale {
                    Some(locale) => format!(
                        "{base}/catalog_{}_{locale}.bin",
                        self.snapshot.resource_version
                    ),
                    None => format!("{base}/catalog_{}.bin", self.snapshot.resource_version),
                };
                if *base != expected
                    || self.catalog_url != stem
                    || self
                        .catalog_locale
                        .as_deref()
                        .is_some_and(|l| !CATALOG_LOCALES.contains(&l))
                {
                    return Err(Error::Verification);
                }
                Ok((
                    base.clone(),
                    Some(format!("{GLOBAL_REMOTE_PLACEHOLDER}/asset/{platform}")),
                ))
            }
        }
    }
}
pub struct CatalogClient {
    config: Config,
    http: Client,
    cdn_http: Client,
    service_download_gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    download_progress: Option<tokio::sync::watch::Sender<jobs::Progress>>,
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
            download_progress: None,
        })
    }

    pub(crate) fn set_download_progress(
        &mut self,
        sender: tokio::sync::watch::Sender<jobs::Progress>,
    ) {
        self.download_progress = Some(sender);
    }
    fn report_download(&self, completed: usize, total: Option<usize>, bytes: u64) {
        if let Some(sender) = &self.download_progress {
            sender.send_replace(jobs::Progress {
                phase: "download".into(),
                completed: completed as u64,
                failed: 0,
                total: total.map(|n| n as u64),
                bytes,
            });
        }
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
        self.report_download(0, None, 0);
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
    /// Basic credentials for `root`, or `None` for an anonymous (`none`) root.
    pub(crate) fn cdn_credentials(&self, root: &str) -> Result<CdnCredentials, Error> {
        let auth = self.config.cdn_roots.get(root).ok_or(Error::Snapshot)?;
        Ok(match auth.authorization {
            CdnAuthorization::Basic => {
                Some((secret(&auth.username_env)?, secret(&auth.credential_env)?))
            }
            CdnAuthorization::None => None,
        })
    }
    pub(crate) fn cdn_get(
        &self,
        url: &str,
        credentials: &CdnCredentials,
    ) -> reqwest::RequestBuilder {
        let request = self.cdn_http.get(url);
        match credentials {
            Some((username, password)) => request.basic_auth(username, Some(password)),
            None => request,
        }
    }
    async fn download_catalog_once(
        &self,
        url: &str,
        credentials: &CdnCredentials,
        path: &Path,
    ) -> Result<(u64, String), Error> {
        self.cdn_attempt(self.download_catalog_inner(url, credentials, path))
            .await
    }
    /// Global catalog `.hash` (the client's catalog version token): bounded, retried like the
    /// catalog, 32 hex digits after trimming.
    async fn catalog_hash(&self, url: &str, credentials: &CdnCredentials) -> Result<String, Error> {
        const MAX: usize = 256;
        let mut attempt = 0;
        loop {
            let result = self
                .cdn_attempt(async {
                    let mut response = self
                        .cdn_get(url, credentials)
                        .send()
                        .await
                        .map_err(|_| Error::Transport)?;
                    status(&response)?;
                    if response.content_length().is_some_and(|n| n > MAX as u64) {
                        return Err(Error::Size);
                    }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
                        if bytes.len() + chunk.len() > MAX {
                            return Err(Error::Size);
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    catalog_hash(&bytes).ok_or(Error::Catalog)
                })
                .await;
            match result {
                Err(error) if self.config.network.catalog_retry.retry(&error, attempt) => {
                    tokio::time::sleep(self.config.network.catalog_retry.delay(attempt)).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }
    async fn download_catalog_inner(
        &self,
        url: &str,
        credentials: &CdnCredentials,
        path: &Path,
    ) -> Result<(u64, String), Error> {
        let mut response = self
            .cdn_get(url, credentials)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        status(&response)?;
        const MAX: u64 = 64 * 1024 * 1024;
        if response.content_length().is_some_and(|s| s > MAX) {
            return Err(Error::Size);
        }
        let mut file = update::staged_file(path)?;
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
        let target = snapshot.target(&self.config, Utc::now())?;
        let url = target.catalog_url.clone();
        let credentials = self.cdn_credentials(&snapshot.snapshot.effective_cdn_root)?;
        // Global: the `.hash` pins the catalog generation. The base catalog's hash must be the
        // one the API reported; it is read again after the download and must not change.
        let catalog_hash = match &target.hash_url {
            Some(hash_url) => {
                tracing::info!(
                    stage = "catalog_hash",
                    region = self.config.region.name(),
                    "Download stage"
                );
                let hash = self.catalog_hash(hash_url, &credentials).await?;
                if self.config.catalog_locale.is_none() && hash != snapshot.snapshot.platform_hash {
                    return Err(Error::Snapshot);
                }
                Some(hash)
            }
            None => None,
        };
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
                .download_catalog_once(&url, &credentials, &staging.path().join("catalog_main.bin"))
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
                self.download_assets(&snapshot, &target, &credentials, staging.path(), config)
                    .await?,
            )
        } else {
            if self.config.refresh_token_env.is_some() {
                self.revalidate(&snapshot).await?;
            } else {
                snapshot.target(&self.config, Utc::now())?;
            }
            None
        };
        if let (Some(hash_url), Some(expected)) = (&target.hash_url, &catalog_hash) {
            tracing::info!(
                stage = "catalog_hash_recheck",
                region = self.config.region.name(),
                "Download stage"
            );
            if self.catalog_hash(hash_url, &credentials).await? != *expected {
                return Err(Error::Snapshot);
            }
        }
        let receipt = Receipt {
            snapshot: snapshot.snapshot,
            catalog_url: url,
            bytes: size,
            sha256,
            downloaded_at: Utc::now(),
            catalog_layout: Some(target.layout),
            bundle_base_url: Some(target.bundle_base_url),
            catalog_locale: self.config.catalog_locale.clone(),
            catalog_hash,
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
/// Basic (username, password) for a CDN root, or `None` when the root is anonymous.
pub(crate) type CdnCredentials = Option<(String, String)>;
fn status(response: &reqwest::Response) -> Result<(), Error> {
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(Error::Status(response.status().as_u16()))
    }
}
async fn write_receipt(dir: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = update::staged_file(&dir.join("receipt.json"))?;
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
pub mod config_env;
pub mod config_source;

mod media_gate;

mod resource_budget;

pub mod cpu_policy;

pub mod stage_limits;

pub mod cpu_throttle;

pub mod storage_sts;

pub mod storage_credentials;

pub mod completion_notify;

pub mod read_policy;
