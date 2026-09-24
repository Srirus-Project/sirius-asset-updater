//! Staged remote resources; nothing is published until the version is rechecked.
use crate::{
    assets::{Asset, AssetConfig, BundleKey, Provider},
    catalog::Catalog,
    secret, status, CatalogClient, Error, SnapshotResponse,
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
        stage: &Path,
        config: &AssetConfig,
    ) -> Result<UpdateReceipt, Error> {
        let catalog_bytes = tokio::fs::read(stage.join("catalog_main.bin"))
            .await
            .map_err(|_| Error::Io)?;
        let catalog_sha256 = hex::encode(Sha256::digest(&catalog_bytes));
        let catalog = Catalog::parse(&catalog_bytes)?;
        let catalog_url = snapshot.catalog_url(&self.config, chrono::Utc::now())?;
        let remote_dir = catalog_url
            .strip_suffix("/catalog_main.bin")
            .ok_or(Error::Snapshot)?;
        let plan = catalog.plan(remote_dir)?;
        let key = config
            .decrypt
            .as_ref()
            .map(|c| BundleKey::from_hex(&secret(&c.key_hex_env)?, &secret(&c.nonce_seed_hex_env)?))
            .transpose()?;
        let auth = self
            .config
            .cdn_roots
            .get(&snapshot.snapshot.effective_cdn_root)
            .ok_or(Error::Snapshot)?;
        let username = secret(&auth.username_env)?;
        let password = secret(&auth.credential_env)?;
        let mut receipt = UpdateReceipt {
            cache_hits: 0,
            assets: Vec::new(),
            embedded_locations: plan.embedded_locations,
            logical_locations: plan.logical_locations,
            total_bytes: 0,
        };
        let mut last_check = tokio::time::Instant::now();
        let mut next = 0;
        while next < plan.assets.len() {
            if last_check.elapsed() >= Duration::from_secs(120) {
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
                let catalog_url = &catalog_url;
                let catalog_sha256 = &catalog_sha256;
                let username = &username;
                let password = &password;
                let key = &key;
                async move {
                    let path = stage.join("assets").join(&asset.relative_path);
                    tokio::fs::create_dir_all(path.parent().ok_or(Error::AssetPath)?)
                        .await
                        .map_err(|_| Error::Io)?;
                    let url = format!("{remote_dir}/{}", asset.relative_path);
                    let cache_id = crate::cache::identity(
                        catalog_url,
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
                        eprintln!("stage=cache_hit resource={}", asset.relative_path);
                    }
                    let mut downloaded = cached;
                    for attempt in 0..3 {
                        if downloaded.is_some() {
                            break;
                        }
                        let result = self
                            .download_asset(&url, username, password, &path, limit, asset)
                            .await;
                        match result {
                            Ok(value) => {
                                downloaded = Some(value);
                                break;
                            }
                            Err(error) => {
                                eprintln!(
                                    "stage=asset_attempt_failed attempt={} resource={} error={}",
                                    attempt + 1,
                                    asset.relative_path,
                                    error
                                );
                                let retry = matches!(
                                    error,
                                    Error::Transport | Error::Status(429 | 500..=599)
                                );
                                if !retry || attempt == 2 {
                                    return Err(error);
                                }
                                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
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
            eprintln!(
                "stage=asset completed={} total={} completed_bytes={}",
                next,
                plan.assets.len(),
                receipt.total_bytes
            );
        }
        // Preserve the provider/dependency graph, not just a list of bundle names.
        let bytes = sonic_rs::to_vec(&catalog).map_err(|_| Error::Catalog)?;
        let mut file = tokio::fs::File::create(stage.join("locations.json"))
            .await
            .map_err(|_| Error::Io)?;
        file.write_all(&bytes).await.map_err(|_| Error::Io)?;
        file.sync_all().await.map_err(|_| Error::Io)?;
        eprintln!("stage=version_recheck");
        self.revalidate(snapshot).await?;
        Ok(receipt)
    }
    async fn download_asset(
        &self,
        url: &str,
        username: &str,
        password: &str,
        path: &Path,
        limit: u64,
        asset: &Asset,
    ) -> Result<(u64, String), Error> {
        let mut response = self
            .http
            .get(url)
            .basic_auth(username, Some(password))
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
