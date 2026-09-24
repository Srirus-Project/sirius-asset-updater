//! Offline receipt verification; never follows asset symlinks or contacts a server.
use crate::{assets::Provider, catalog::Catalog, Error, Receipt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;
#[derive(Serialize)]
pub struct Verification {
    pub catalog_verified: bool,
    pub asset_files_verified: usize,
    pub asset_bytes_verified: u64,
    pub planned_remote_files: usize,
    pub embedded_locations: usize,
    pub decrypted_bundles: usize,
}
async fn file(root: &Path, relative: &str) -> Result<PathBuf, Error> {
    if !crate::assets::safe_relative(relative) {
        return Err(Error::Verification);
    }
    let mut path = root.to_path_buf();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        path.push(part);
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| Error::Verification)?;
        if if parts.peek().is_some() {
            !meta.is_dir()
        } else {
            !meta.is_file()
        } {
            return Err(Error::Verification);
        }
    }
    Ok(path)
}
async fn read(path: &Path, max: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::Verification)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::Verification)?;
    if bytes.len() > max {
        return Err(Error::Verification);
    }
    Ok(bytes)
}
async fn hash(path: &Path, expected_size: u64) -> Result<(String, Vec<u8>), Error> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::Verification)?;
    if file
        .metadata()
        .await
        .map_err(|_| Error::Verification)?
        .len()
        != expected_size
    {
        return Err(Error::Verification);
    }
    let mut bytes = vec![0; 65536];
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut prefix = Vec::new();
    loop {
        let len = file
            .read(&mut bytes)
            .await
            .map_err(|_| Error::Verification)?;
        if len == 0 {
            break;
        }
        count += len as u64;
        if count > expected_size {
            return Err(Error::Verification);
        }
        let take = (8 - prefix.len()).min(len);
        prefix.extend_from_slice(&bytes[..take]);
        digest.update(&bytes[..len]);
    }
    if count != expected_size {
        return Err(Error::Verification);
    }
    Ok((hex::encode(digest.finalize()), prefix))
}
pub async fn verify(directory: &Path) -> Result<Verification, Error> {
    let root = tokio::fs::canonicalize(directory)
        .await
        .map_err(|_| Error::Verification)?;
    let receipt: Receipt =
        sonic_rs::from_slice(&read(&file(&root, "receipt.json").await?, 64 * 1024 * 1024).await?)
            .map_err(|_| Error::Verification)?;
    let bytes = read(&file(&root, "catalog_main.bin").await?, 64 * 1024 * 1024).await?;
    if bytes.len() as u64 != receipt.bytes || hex::encode(Sha256::digest(&bytes)) != receipt.sha256
    {
        return Err(Error::Verification);
    }
    let catalog = Catalog::parse(&bytes)?;
    let remote = receipt
        .catalog_url
        .strip_suffix("/catalog_main.bin")
        .ok_or(Error::Verification)?;
    let plan = catalog.plan(remote)?;
    let mut result = Verification {
        catalog_verified: true,
        asset_files_verified: 0,
        asset_bytes_verified: 0,
        planned_remote_files: plan.assets.len(),
        embedded_locations: plan.embedded_locations,
        decrypted_bundles: 0,
    };
    if let Some(update) = receipt.update {
        if update.assets.len() != plan.assets.len()
            || update.embedded_locations != plan.embedded_locations
            || update.logical_locations != plan.logical_locations
        {
            return Err(Error::Verification);
        }
        // The serialized graph must still be exactly the graph decoded from this catalog.
        let graph = sonic_rs::to_vec(&catalog).map_err(|_| Error::Verification)?;
        let (actual, _) = hash(&file(&root, "locations.json").await?, graph.len() as u64).await?;
        if actual != hex::encode(Sha256::digest(&graph)) {
            return Err(Error::Verification);
        }
        let mut expected: BTreeMap<_, _> = plan
            .assets
            .into_iter()
            .map(|a| (a.relative_path, a.provider))
            .collect();
        for asset in update.assets {
            if expected.remove(&asset.relative_path) != Some(asset.provider)
                || asset.bytes == 0
                || asset.bytes > 2 * 1024 * 1024 * 1024
                || (asset.decrypted && asset.provider != Provider::EncryptedBundle)
                || (!asset.decrypted && asset.stored_sha256 != asset.downloaded_sha256)
            {
                return Err(Error::Verification);
            }
            let path = file(&root, &format!("assets/{}", asset.relative_path)).await?;
            let (actual, prefix) = hash(&path, asset.bytes).await?;
            if actual != asset.stored_sha256
                || ((asset.decrypted || asset.provider == Provider::UnityBundle)
                    && prefix != b"UnityFS\0")
            {
                return Err(Error::Verification);
            }
            result.asset_bytes_verified = result
                .asset_bytes_verified
                .checked_add(asset.bytes)
                .ok_or(Error::Verification)?;
            if result.asset_bytes_verified > 128 * 1024 * 1024 * 1024 {
                return Err(Error::Verification);
            }
            result.asset_files_verified += 1;
            result.decrypted_bundles += usize::from(asset.decrypted);
        }
        if result.asset_bytes_verified != update.total_bytes {
            return Err(Error::Verification);
        }
    }
    Ok(result)
}
