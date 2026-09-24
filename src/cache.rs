//! Version/catalog-scoped ciphertext cache. Cache entries are never publications.
use crate::{assets::Provider, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Serialize, Deserialize)]
struct Metadata {
    bytes: u64,
    sha256: String,
}
pub(crate) fn identity(
    catalog_url: &str,
    catalog_sha256: &str,
    relative: &str,
    provider: Provider,
) -> String {
    let mut hash = Sha256::new();
    for value in [
        catalog_url,
        catalog_sha256,
        relative,
        match provider {
            Provider::EncryptedBundle => "encrypted",
            Provider::UnityBundle => "unity",
            Provider::Cri => "cri",
        },
    ] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())
}
pub(crate) async fn restore(
    root: &Path,
    id: &str,
    destination: &Path,
    limit: u64,
) -> Result<Option<(u64, String)>, Error> {
    let entry = root.join(id);
    if !regular_directory(&entry).await {
        return Ok(None);
    }
    let meta_path = entry.join("metadata.json");
    if !regular_file(&meta_path).await {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(meta_path)
        .await
        .map_err(|_| Error::Io)?
        .take(4097)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::Io)?;
    if bytes.len() > 4096 {
        return Ok(None);
    }
    let Ok(metadata) = sonic_rs::from_slice::<Metadata>(&bytes) else {
        return Ok(None);
    };
    if metadata.bytes == 0 || metadata.bytes > limit || metadata.sha256.len() != 64 {
        return Ok(None);
    }
    let source = entry.join("data");
    if !regular_file(&source).await {
        return Ok(None);
    }
    let mut source = tokio::fs::File::open(source).await.map_err(|_| Error::Io)?;
    if source.metadata().await.map_err(|_| Error::Io)?.len() != metadata.bytes {
        return Ok(None);
    }
    let mut target = tokio::fs::File::create(destination)
        .await
        .map_err(|_| Error::Io)?;
    let mut buffer = vec![0; 65536];
    let mut digest = Sha256::new();
    let mut size = 0u64;
    loop {
        let read = source.read(&mut buffer).await.map_err(|_| Error::Io)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > metadata.bytes {
            return Ok(None);
        }
        digest.update(&buffer[..read]);
        target
            .write_all(&buffer[..read])
            .await
            .map_err(|_| Error::Io)?;
    }
    if size != metadata.bytes || hex::encode(digest.finalize()) != metadata.sha256 {
        return Ok(None);
    }
    target.sync_all().await.map_err(|_| Error::Io)?;
    Ok(Some((size, metadata.sha256)))
}
async fn regular_directory(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.is_dir())
}
async fn regular_file(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.is_file())
}
pub(crate) struct Pending {
    directory: tempfile::TempDir,
    target: PathBuf,
}
impl Pending {
    /// Capture downloaded bytes before in-place decryption. Commit only after the
    /// selected provider's validation succeeds; errors/cancellation discard this copy.
    pub(crate) async fn prepare(
        root: &Path,
        id: &str,
        source: &Path,
        size: u64,
        sha256: &str,
    ) -> Result<Self, Error> {
        tokio::fs::create_dir_all(root)
            .await
            .map_err(|_| Error::Io)?;
        let directory = tempfile::Builder::new()
            .prefix(".pending-")
            .tempdir_in(root)
            .map_err(|_| Error::Io)?;
        tokio::fs::copy(source, directory.path().join("data"))
            .await
            .map_err(|_| Error::Io)?;
        tokio::fs::File::open(directory.path().join("data"))
            .await
            .map_err(|_| Error::Io)?
            .sync_all()
            .await
            .map_err(|_| Error::Io)?;
        let metadata = sonic_rs::to_vec(&Metadata {
            bytes: size,
            sha256: sha256.into(),
        })
        .map_err(|_| Error::Io)?;
        let mut file = tokio::fs::File::create(directory.path().join("metadata.json"))
            .await
            .map_err(|_| Error::Io)?;
        file.write_all(&metadata).await.map_err(|_| Error::Io)?;
        file.sync_all().await.map_err(|_| Error::Io)?;
        Ok(Self {
            directory,
            target: root.join(id),
        })
    }
    pub(crate) async fn commit(self) -> Result<(), Error> {
        // Replacing a corrupt managed entry is safe: the name is a computed SHA-256,
        // never a path from the catalog. There is no shared mutable publication here.
        if let Ok(meta) = tokio::fs::symlink_metadata(&self.target).await {
            if meta.is_dir() {
                tokio::fs::remove_dir_all(&self.target)
                    .await
                    .map_err(|_| Error::Io)?;
            } else {
                tokio::fs::remove_file(&self.target)
                    .await
                    .map_err(|_| Error::Io)?;
            }
        }
        tokio::fs::rename(self.directory.path(), &self.target)
            .await
            .map_err(|_| Error::Io)
    }
}

pub(crate) struct Guard {
    _file: std::fs::File,
}
impl Guard {
    pub(crate) fn acquire(root: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(root).map_err(|_| Error::Io)?;
        let path = root.join(".lock");
        if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            return Err(Error::Io);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| Error::Io)?;
        file.try_lock().map_err(|_| Error::Busy)?;
        Ok(Self { _file: file })
    }
}
