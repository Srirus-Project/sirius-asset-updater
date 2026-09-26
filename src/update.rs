//! Staged remote resources; nothing is published until the version is rechecked.
use crate::{
    assets::{Asset, AssetConfig, BundleKey, Provider},
    catalog::Catalog,
    secret, status, CatalogClient, CatalogTarget, CdnCredentials, Error, SnapshotResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Serialize, Deserialize)]
pub struct AssetReceipt {
    pub relative_path: String,
    pub provider: Provider,
    pub bytes: u64,
    pub downloaded_sha256: String,
    pub stored_sha256: String,
    pub decrypted: bool,
}
#[derive(Serialize, Deserialize)]
pub struct UpdateReceipt {
    #[serde(default)]
    pub selection: crate::catalog::Selection,
    #[serde(default)]
    pub cache_hits: usize,
    pub assets: Vec<AssetReceipt>,
    pub embedded_locations: usize,
    pub logical_locations: usize,
    pub total_bytes: u64,
}
impl CatalogClient {
    pub(crate) async fn download_assets(
        &self,
        snapshot: &SnapshotResponse,
        target: &CatalogTarget,
        credentials: &CdnCredentials,
        stage: &Path,
        config: &AssetConfig,
    ) -> Result<UpdateReceipt, Error> {
        let catalog_bytes = tokio::fs::read(stage.join("catalog_main.bin"))
            .await
            .map_err(|_| Error::Io)?;
        let catalog_sha256 = hex::encode(Sha256::digest(&catalog_bytes));
        let catalog = Catalog::parse(&catalog_bytes)?;
        // Re-validate freshness; the bundle directory comes from the layout, not the URL text.
        if snapshot.target(&self.config, chrono::Utc::now())? != *target {
            return Err(Error::Snapshot);
        }
        let catalog_url = &target.catalog_url;
        let remote_dir = target.bundle_base_url.as_str();
        let mut plan = catalog
            .select(&config.selection)?
            .plan_with(remote_dir, target.remote_placeholder.as_deref())?;
        config.selection.prioritize(&mut plan)?;
        self.report_download(0, Some(plan.assets.len()), 0);
        let key = config
            .decrypt
            .as_ref()
            .map(|c| BundleKey::from_hex(&secret(&c.key_hex_env)?, &secret(&c.nonce_seed_hex_env)?))
            .transpose()?;
        let mut receipt = UpdateReceipt {
            selection: config.selection.clone(),
            cache_hits: 0,
            assets: Vec::new(),
            embedded_locations: plan.embedded_locations,
            logical_locations: plan.logical_locations,
            total_bytes: 0,
        };
        let mut last_check = tokio::time::Instant::now();
        let mut next = 0;
        while next < plan.assets.len() {
            if last_check.elapsed()
                >= Duration::from_millis(self.config.network.revalidate_interval_ms)
            {
                self.revalidate(snapshot).await?;
                last_check = tokio::time::Instant::now();
            }
            let remaining = config
                .max_total_bytes
                .checked_sub(receipt.total_bytes)
                .ok_or(Error::Size)?;
            if remaining == 0 {
                return Err(Error::Size);
            }
            // Reserve each worker's worst-case file size before dispatch. Near the
            // total budget, fall back to one worker so small files can still fit.
            let workers = config
                .concurrency
                .min((remaining / config.max_file_bytes).max(1) as usize);
            let end = (next + workers).min(plan.assets.len());
            let limit = remaining.min(config.max_file_bytes);
            let tasks = plan.assets[next..end].iter().map(|asset| {
                let catalog_sha256 = &catalog_sha256;
                let key = &key;
                async move {
                    let path = stage.join("assets").join(&asset.relative_path);
                    tokio::fs::create_dir_all(path.parent().ok_or(Error::AssetPath)?)
                        .await
                        .map_err(|_| Error::Io)?;
                    let url = format!("{remote_dir}/{}", asset.relative_path);
                    let scoped_catalog = format!(
                        "{}:{}:{}:{}",
                        self.config.region.name(),
                        self.config.environment,
                        self.config.platform().name(),
                        catalog_url
                    );
                    let cache_id = crate::cache::identity(
                        &scoped_catalog,
                        catalog_sha256,
                        &asset.relative_path,
                        asset.provider,
                    );
                    let cached = if let Some(root) = &config.cache_directory {
                        crate::cache::restore(root, &cache_id, &path, limit).await?
                    } else {
                        None
                    };
                    let cache_hit = cached.is_some();
                    if cache_hit {
                        tracing::debug!(stage = "cache_hit", "Verified download cache hit");
                    }
                    let mut downloaded = cached;
                    for attempt in 0..self.config.network.asset_retry.attempts {
                        if downloaded.is_some() {
                            break;
                        }
                        let result = self
                            .download_asset(&url, credentials, &path, limit, asset)
                            .await;
                        match result {
                            Ok(value) => {
                                downloaded = Some(value);
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    stage = "asset_attempt_failed",
                                    attempt = attempt + 1,
                                    error_code = error.code(),
                                    status = error.http_status(),
                                    "Asset request failed"
                                );
                                if !self.config.network.asset_retry.retry(&error, attempt) {
                                    return Err(error);
                                }
                                tokio::time::sleep(self.config.network.asset_retry.delay(attempt))
                                    .await;
                            }
                        }
                    }
                    let (bytes, downloaded_sha256) = downloaded.ok_or(Error::Transport)?;
                    let pending = if !cache_hit {
                        if let Some(root) = &config.cache_directory {
                            Some(
                                crate::cache::Pending::prepare(
                                    root,
                                    &cache_id,
                                    &path,
                                    bytes,
                                    &downloaded_sha256,
                                )
                                .await?,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let mut decrypted = false;
                    if let Some(key) = key
                        .as_ref()
                        .filter(|_| asset.provider == Provider::EncryptedBundle)
                    {
                        let basename = asset
                            .relative_path
                            .rsplit('/')
                            .next()
                            .ok_or(Error::AssetPath)?;
                        decrypted = key.decrypt_file(&path, basename).await?;
                    }
                    let stored_sha256 = if decrypted {
                        file_hash(&path).await?
                    } else {
                        downloaded_sha256.clone()
                    };
                    if let Some(pending) = pending {
                        pending.commit().await?;
                    }
                    Ok::<_, Error>((
                        AssetReceipt {
                            relative_path: asset.relative_path.clone(),
                            provider: asset.provider,
                            bytes,
                            downloaded_sha256,
                            stored_sha256,
                            decrypted,
                        },
                        cache_hit,
                    ))
                }
            });
            let completed = futures_util::future::try_join_all(tasks).await?;
            for (asset, cache_hit) in completed {
                receipt.total_bytes += asset.bytes;
                receipt.cache_hits += usize::from(cache_hit);
                receipt.assets.push(asset);
            }
            next = end;
            self.report_download(next, Some(plan.assets.len()), receipt.total_bytes);
            tracing::info!(
                stage = "asset",
                completed = next,
                total = plan.assets.len(),
                bytes = receipt.total_bytes,
                "Asset download progress"
            );
        }
        // Preserve the provider/dependency graph, not just a list of bundle names.
        let bytes = sonic_rs::to_vec(&catalog).map_err(|_| Error::Catalog)?;
        let mut file = tokio::fs::File::create(stage.join("locations.json"))
            .await
            .map_err(|_| Error::Io)?;
        file.write_all(&bytes).await.map_err(|_| Error::Io)?;
        file.sync_all().await.map_err(|_| Error::Io)?;
        tracing::info!(stage = "version_recheck", "Rechecking download version");
        self.revalidate(snapshot).await?;
        Ok(receipt)
    }
    async fn download_asset(
        &self,
        url: &str,
        credentials: &CdnCredentials,
        path: &Path,
        limit: u64,
        asset: &Asset,
    ) -> Result<(u64, String), Error> {
        self.cdn_attempt(self.download_asset_inner(url, credentials, path, limit, asset))
            .await
    }
    async fn download_asset_inner(
        &self,
        url: &str,
        credentials: &CdnCredentials,
        path: &Path,
        limit: u64,
        asset: &Asset,
    ) -> Result<(u64, String), Error> {
        let mut response = self
            .cdn_get(url, credentials)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        status(&response)?;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(Error::Size);
        }
        // The entire run lives in a private TempDir. Retried requests truncate this
        // unpublished file; no Range continuation or mixed-generation append.
        let mut file = tokio::fs::File::create(path).await.map_err(|_| Error::Io)?;
        let mut size = 0;
        let mut hash = Sha256::new();
        let mut prefix = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            size += chunk.len() as u64;
            if size > limit {
                return Err(Error::Size);
            }
            let take = (8 - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
            hash.update(&chunk);
            file.write_all(&chunk).await.map_err(|_| Error::Io)?;
        }
        if size == 0 || (asset.provider == Provider::UnityBundle && prefix != b"UnityFS\0") {
            return Err(Error::Bundle);
        }
        file.sync_all().await.map_err(|_| Error::Io)?;
        Ok((size, hex::encode(hash.finalize())))
    }
}
async fn file_hash(path: &Path) -> Result<String, Error> {
    let mut file = tokio::fs::File::open(path).await.map_err(|_| Error::Io)?;
    let mut buffer = vec![0; 65536];
    let mut hash = Sha256::new();
    loop {
        let count = file.read(&mut buffer).await.map_err(|_| Error::Io)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}
