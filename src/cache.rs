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
            .prefix(".pending-download-")
            .tempdir_in(root)
            .map_err(|_| Error::Io)?;
        tokio::fs::copy(source, directory.path().join("data"))
            .await
            .map_err(|_| Error::Io)?;
        // Windows FlushFileBuffers requires a writable handle, even for a
        // file just copied successfully. Opening with write does not truncate it.
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(directory.path().join("data"))
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
        sync_directory(directory.path()).await?;
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
            .map_err(|_| Error::Io)?;
        sync_directory(self.target.parent().ok_or(Error::Io)?).await
    }
}

async fn sync_directory(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::Io)?
        .sync_all()
        .await
        .map_err(|_| Error::Io)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) struct Guard {
    _file: std::fs::File,
}
impl Drop for Guard {
    fn drop(&mut self) {
        // Release ownership explicitly before close. A concurrent child spawn can briefly
        // inherit the open file description before exec closes its descriptors.
        let _ = self._file.unlock();
    }
}
impl Guard {
    /// Reap only this download cache's generated temporary names, while exclusively owned.
    pub(crate) fn acquire_download(root: &Path) -> Result<Self, Error> {
        let guard = Self::acquire(root)?;
        for entry in std::fs::read_dir(root).map_err(|_| Error::Io)? {
            let entry = entry.map_err(|_| Error::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // tempfile uses six alphanumeric suffix characters. Retain unrelated names,
            // including decoded-export cache staging and operator-created directories.
            let suffix = name
                .strip_prefix(".pending-download-")
                .or_else(|| name.strip_prefix(".pending-"));
            if !suffix.is_some_and(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
            {
                continue;
            }
            let kind = entry.file_type().map_err(|_| Error::Io)?;
            if kind.is_dir() {
                std::fs::remove_dir_all(entry.path()).map_err(|_| Error::Io)?;
            } else if kind.is_file() || kind.is_symlink() {
                std::fs::remove_file(entry.path()).map_err(|_| Error::Io)?;
            }
        }
        Ok(guard)
    }
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

#[cfg(all(test, unix))]
mod guard_tests {
    use super::*;
    #[test]
    fn download_cache_recovers_after_process_kill() {
        const ENV: &str = "SIRIUS_TEST_CACHE_CRASH_DIRECTORY";
        if let Some(root) = std::env::var_os(ENV) {
            let root = PathBuf::from(root);
            let _guard = Guard::acquire_download(&root).unwrap();
            let source = root.parent().unwrap().join("source");
            let digest = hex::encode(Sha256::digest(b"verified resource"));
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                Pending::prepare(&root, &"a".repeat(64), &source, 17, &digest)
                    .await
                    .unwrap()
                    .commit()
                    .await
                    .unwrap();
                let _pending = Pending::prepare(&root, &"b".repeat(64), &source, 17, &digest)
                    .await
                    .unwrap();
                std::fs::write(root.parent().unwrap().join("ready"), b"ready").unwrap();
                std::thread::sleep(std::time::Duration::from_secs(60));
            });
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        std::fs::write(temp.path().join("source"), b"verified resource").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cache::guard_tests::download_cache_recovers_after_process_kill",
                "--nocapture",
            ])
            .env(ENV, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !temp.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ready = temp.path().join("ready").exists();
        let busy = matches!(Guard::acquire_download(&root), Err(Error::Busy));
        let killed = child.kill();
        let status = child.wait().unwrap();
        assert!(ready, "child never prepared crash fixture");
        assert!(busy);
        killed.unwrap();
        assert!(!status.success());
        // Unknown directories and decoded-export staging are not download-owned names.
        for name in [".pending-operator", ".pending-export-Ab1234"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::create_dir(root.join(".pending-Ab1234")).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"unrelated").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".pending-download-Cd5678")).unwrap();
        let _guard = Guard::acquire_download(&root).unwrap();
        assert!(!root.join(".pending-Ab1234").exists());
        assert!(!root.join(".pending-download-Cd5678").exists());
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"unrelated");
        for name in [".pending-operator", ".pending-export-Ab1234"] {
            assert!(root.join(name).is_dir());
        }
        assert!(!std::fs::read_dir(&root).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pending-download-")));
        let destination = temp.path().join("restored");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(runtime
            .block_on(restore(&root, &"a".repeat(64), &destination, 1024))
            .unwrap()
            .is_some());
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified resource");
        assert!(runtime
            .block_on(restore(&root, &"b".repeat(64), &destination, 1024))
            .unwrap()
            .is_none());
    }
    #[test]
    fn ownership_ends_even_if_a_duplicate_descriptor_remains_open() {
        let root = tempfile::tempdir().unwrap();
        let guard = Guard::acquire(root.path()).unwrap();
        // Models the shared open file description inherited between fork and exec.
        let inherited = guard._file.try_clone().unwrap();
        assert!(matches!(Guard::acquire(root.path()), Err(Error::Busy)));
        drop(guard);
        let next = Guard::acquire(root.path()).unwrap();
        drop(inherited);
        assert!(matches!(Guard::acquire(root.path()), Err(Error::Busy)));
        drop(next);
        assert!(Guard::acquire(root.path()).is_ok());
    }
}
