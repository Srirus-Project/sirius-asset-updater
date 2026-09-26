//! Offline configuration checks and bounded API version refreshes.
use crate::{assets::BundleKey, secret, status, CatalogClient, Config, Error, SnapshotResponse};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

#[derive(Serialize)]
pub struct CheckReport {
    pub ready: bool,
    pub missing_env: BTreeSet<String>,
    pub invalid_fields: Vec<&'static str>,
    pub refresh_enabled: bool,
    pub assets_enabled: bool,
    pub decryption_enabled: bool,
}
impl Config {
    /// No HTTP requests and no secret values in the result.
    pub fn check(&self) -> Result<CheckReport, Error> {
        self.validate()?;
        Ok(self.check_secrets())
    }
    pub(crate) fn check_secrets(&self) -> CheckReport {
        let mut report = CheckReport {
            ready: true,
            missing_env: BTreeSet::new(),
            invalid_fields: Vec::new(),
            refresh_enabled: self.refresh_token_env.is_some(),
            assets_enabled: self.assets.is_some(),
            decryption_enabled: self.assets.as_ref().is_some_and(|c| c.decrypt.is_some()),
        };
        let mut names = vec![&self.internal_token_env];
        if let Some(name) = &self.refresh_token_env {
            names.push(name);
        }
        // Anonymous (`none`) roots have no CDN secrets to check.
        for auth in self.cdn_roots.values() {
            if auth.authorization == crate::CdnAuthorization::Basic {
                names.extend([&auth.username_env, &auth.credential_env]);
            }
        }
        if let Some(decrypt) = self.assets.as_ref().and_then(|c| c.decrypt.as_ref()) {
            names.extend([&decrypt.key_hex_env, &decrypt.nonce_seed_hex_env]);
        }
        for proxy in self
            .network
            .api_proxy
            .iter()
            .chain(self.network.cdn_proxy.iter())
        {
            names.push(&proxy.url_env);
            names.extend(proxy.authorization_env.iter());
        }
        for name in names {
            if secret(name).is_err() {
                report.missing_env.insert(name.clone());
            }
        }
        for name in [&self.internal_token_env]
            .into_iter()
            .chain(self.refresh_token_env.iter())
        {
            if let Ok(value) = secret(name) {
                if !value.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                    report.invalid_fields.push("bearer_token");
                }
            }
        }
        if let Some(name) = &self.refresh_token_env {
            if let (Ok(api), Ok(internal)) = (secret(name), secret(&self.internal_token_env)) {
                if api == internal {
                    report
                        .invalid_fields
                        .push("api_and_internal_tokens_must_differ");
                }
            }
        }
        for auth in self.cdn_roots.values() {
            if secret(&auth.username_env)
                .is_ok_and(|value| value.contains(':') || value.chars().any(char::is_control))
            {
                report.invalid_fields.push("cdn_username");
            }
        }
        if let Some(decrypt) = self.assets.as_ref().and_then(|c| c.decrypt.as_ref()) {
            if let (Ok(key), Ok(seed)) = (
                secret(&decrypt.key_hex_env),
                secret(&decrypt.nonce_seed_hex_env),
            ) {
                if BundleKey::from_hex(&key, &seed).is_err() {
                    report.invalid_fields.push("bundle_key_or_nonce_seed");
                }
            }
        }
        for (field, proxy) in [
            ("network.api_proxy", &self.network.api_proxy),
            ("network.cdn_proxy", &self.network.cdn_proxy),
        ] {
            if let Some(proxy) = proxy {
                if matches!(proxy.resolve(), Err(Error::Config)) {
                    report.invalid_fields.push(field);
                }
            }
        }
        report.ready = report.missing_env.is_empty() && report.invalid_fields.is_empty();
        report
    }
}
impl CatalogClient {
    async fn refresh_version(&self) -> Result<(), Error> {
        let Some(name) = &self.config.refresh_token_env else {
            return Ok(());
        };
        let url = self.config.api_url(false, "system");
        let response = self
            .http
            .get(url)
            .bearer_auth(secret(name)?)
            .timeout(Duration::from_millis(
                self.config.network.refresh_timeout_ms,
            ))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let bytes = bounded_api_response(response).await?;
        #[derive(Deserialize)]
        struct System {
            status: String,
        }
        let system: System = sonic_rs::from_slice(&bytes).map_err(|_| Error::Snapshot)?;
        if system.status != "available" {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    /// Refresh and read are separate scoped requests: neither token reaches a CDN.
    pub(crate) async fn observed_snapshot(&self) -> Result<SnapshotResponse, Error> {
        for attempt in 0..self.config.network.snapshot_retry.attempts {
            let result = async {
                self.refresh_version().await?;
                let snapshot = self.read_snapshot().await?;
                snapshot.catalog_url(&self.config, chrono::Utc::now())?;
                Ok(snapshot)
            }
            .await;
            match result {
                Err(error) if self.config.network.snapshot_retry.retry(&error, attempt) => {
                    tracing::warn!(
                        stage = "snapshot_retry",
                        attempt = attempt + 1,
                        error_code = error.code(),
                        status = error.http_status(),
                        "Snapshot request will retry"
                    );
                    tokio::time::sleep(self.config.network.snapshot_retry.delay(attempt)).await;
                }
                value => return value,
            }
        }
        unreachable!("last attempt always returns")
    }
    pub(crate) async fn revalidate(&self, pinned: &SnapshotResponse) -> Result<(), Error> {
        let current = self.observed_snapshot().await?;
        let old = &pinned.snapshot;
        let new = &current.snapshot;
        if old.region != new.region
            || old.environment != new.environment
            || old.client_version != new.client_version
            || old.resource_version != new.resource_version
            || old.platform_hash != new.platform_hash
            || old.platform != new.platform
            || old.protocol_version != new.protocol_version
            || old.effective_cdn_root != new.effective_cdn_root
            || old.credential_ref != new.credential_ref
            || old.schema_version != new.schema_version
            || old.catalog_layout != new.catalog_layout
            || old.catalog_url != new.catalog_url
            || old.bundle_base_url != new.bundle_base_url
            || old.cdn_authorization != new.cdn_authorization
        {
            return Err(Error::Snapshot);
        }
        Ok(())
    }
    /// Queries only the configured Game API; does not request a catalog or assets.
    pub async fn probe(&self) -> Result<SnapshotResponse, Error> {
        if !self.config.check_secrets().ready {
            return Err(Error::Preflight);
        }
        self.observed_snapshot().await
    }
}
pub(crate) async fn bounded_api_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, Error> {
    status(&response)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if bytes.len() + chunk.len() > 65536 {
            return Err(Error::Size);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
