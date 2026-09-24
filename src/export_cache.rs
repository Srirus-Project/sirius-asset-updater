//! Private, content-addressed decoded resource cache. Never a publication itself.
use super::{err, ExportConfig, ResourceReport};
use crate::{update::AssetReceipt, Error, Snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAX_REPORT: u64 = 16 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    schema: u8,
    identity: String,
    report_sha256: String,
    report: ResourceReport,
}
pub(super) struct Cache {
    root: PathBuf,
    scope: String,
    limit: u64,
    cancel: Arc<AtomicBool>,
    _guard: crate::cache::Guard,
}
impl Cache {
    pub(super) fn open(
        config: &ExportConfig,
        input: &Path,
        output: &Path,
        snapshot: &Snapshot,
        key: u64,
    ) -> Result<Option<Self>, Error> {
        let Some(root) = &config.cache_directory else {
            return Ok(None);
        };
        fs::create_dir_all(root).map_err(err)?;
        if !fs::symlink_metadata(root).map_err(err)?.is_dir() {
            return Err(Error::Config);
        }
        let root = fs::canonicalize(root).map_err(err)?;
        if [input, output]
            .iter()
            .any(|p| root.starts_with(p) || p.starts_with(&root))
        {
            return Err(Error::Config);
        }
        let guard = crate::cache::Guard::acquire(&root)?;
        let binary = fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?;
        let ffmpeg = executable(&config.ffmpeg)?;
        let split = config
            .split_acb_xor_env
            .as_ref()
            .map(|name| std::env::var(name).map_err(|_| Error::Secret))
            .transpose()?;
        // Only this digest reaches entry names/metadata. Never persist secret values.
        let scope = fingerprint(&(
            1u8,
            snapshot.region.unwrap_or_default(),
            &snapshot.environment,
            (&snapshot.platform, &snapshot.effective_cdn_root),
            &snapshot.client_version,
            &snapshot.protocol_version,
            &config.selection,
            &config.image,
            config.audio,
            key,
            split,
            &config.cache_revision,
            file_hash(&binary, &config.cancel)?,
            file_hash(&ffmpeg, &config.cancel)?,
        ))?;
        Ok(Some(Self {
            root,
            scope,
            limit: config.max_resource_output_bytes,
            cancel: config.cancel.clone(),
            _guard: guard,
        }))
    }
    pub(super) fn identity(
        &self,
        asset: &AssetReceipt,
        dependencies: &BTreeMap<String, String>,
    ) -> Result<String, Error> {
        fingerprint(&(
            &self.scope,
            &asset.relative_path,
            asset.provider,
            &asset.stored_sha256,
            asset.decrypted,
            dependencies,
        ))
    }
    pub(super) fn process(
        &self,
        id: &str,
        output: &Path,
        index: usize,
        decode: impl FnOnce() -> Result<ResourceReport, Error>,
    ) -> Result<ResourceReport, Error> {
        self.check_cancel()?;
        if let Some(report) = self.restore(id, output, index)? {
            return Ok(report);
        }
        let report = decode()?;
        if report.errors.is_empty() {
            self.check_cancel()?;
            self.store(id, &output.join(&report.output_directory), &report)?;
        }
        Ok(report)
    }
    fn check_cancel(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    fn restore(
        &self,
        id: &str,
        output: &Path,
        index: usize,
    ) -> Result<Option<ResourceReport>, Error> {
        let source = self.root.join(id);
        if !regular(&source, true) || !regular(&source.join("entry.json"), false) {
            return Ok(None);
        }
        let mut bytes = vec![];
        fs::File::open(source.join("entry.json"))
            .map_err(err)?
            .take(MAX_REPORT + 1)
            .read_to_end(&mut bytes)
            .map_err(err)?;
        if bytes.len() as u64 > MAX_REPORT {
            return Ok(None);
        }
        let Ok(entry) = sonic_rs::from_slice::<Entry>(&bytes) else {
            return Ok(None);
        };
        if entry.schema != 1
            || entry.identity != id
            || !entry.report.errors.is_empty()
            || entry.report_sha256 != fingerprint(&entry.report)?
        {
            return Ok(None);
        }
        let work = tempfile::Builder::new()
            .prefix(".reuse-")
            .tempdir_in(output)
            .map_err(err)?;
        if !self.copy_outputs(&source.join("data"), work.path(), &entry.report)? {
            return Ok(None);
        }
        self.check_cancel()?;
        let mut report = entry.report;
        report.output_directory = format!("{index:05}");
        report.cache_hit = true;
        fs::rename(work.path(), output.join(&report.output_directory)).map_err(err)?;
        Ok(Some(report))
    }
    fn store(&self, id: &str, source: &Path, report: &ResourceReport) -> Result<(), Error> {
        let work = tempfile::Builder::new()
            .prefix(".pending-export-")
            .tempdir_in(&self.root)
            .map_err(err)?;
        let data = work.path().join("data");
        fs::create_dir(&data).map_err(err)?;
        if !self.copy_outputs(source, &data, report)? {
            return Err(Error::Verification);
        }
        let bytes = sonic_rs::to_vec(&Entry {
            schema: 1,
            identity: id.into(),
            report_sha256: fingerprint(report)?,
            report: report.clone(),
        })
        .map_err(err)?;
        if bytes.len() as u64 > MAX_REPORT {
            return Err(Error::Size);
        }
        let mut metadata = fs::File::create(work.path().join("entry.json")).map_err(err)?;
        metadata.write_all(&bytes).map_err(err)?;
        metadata.sync_all().map_err(err)?;
        self.check_cancel()?;
        let target = self.root.join(id);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.is_dir() {
                fs::remove_dir_all(&target).map_err(err)?;
            } else {
                fs::remove_file(&target).map_err(err)?;
            }
        }
        fs::rename(work.path(), target).map_err(err)
    }
    fn copy_outputs(
        &self,
        source: &Path,
        destination: &Path,
        report: &ResourceReport,
    ) -> Result<bool, Error> {
        if !regular(source, true) {
            return Ok(false);
        }
        let mut total = 0u64;
        let mut paths = std::collections::BTreeSet::new();
        for item in &report.outputs {
            self.check_cancel()?;
            if !safe_path(&item.path) || !paths.insert(&item.path) || !digest(&item.sha256) {
                return Ok(false);
            }
            total = total.checked_add(item.bytes).ok_or(Error::Size)?;
            if total > self.limit {
                return Ok(false);
            }
            let mut path = source.to_owned();
            let components: Vec<_> = item.path.split('/').collect();
            for (i, part) in components.iter().enumerate() {
                path.push(part);
                if !regular(&path, i + 1 != components.len()) {
                    return Ok(false);
                }
            }
            let mut file = fs::File::open(&path).map_err(err)?;
            if file.metadata().map_err(err)?.len() != item.bytes {
                return Ok(false);
            }
            let target = destination.join(&item.path);
            fs::create_dir_all(target.parent().ok_or(Error::AssetPath)?).map_err(err)?;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(err)?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                self.check_cancel()?;
                let n = file.read(&mut buffer).map_err(err)?;
                if n == 0 {
                    break;
                }
                size = size.checked_add(n as u64).ok_or(Error::Size)?;
                if size > item.bytes {
                    return Ok(false);
                }
                hash.update(&buffer[..n]);
                out.write_all(&buffer[..n]).map_err(err)?;
            }
            if size != item.bytes || hex::encode(hash.finalize()) != item.sha256 {
                return Ok(false);
            }
            out.sync_all().map_err(err)?;
        }
        Ok(true)
    }
}
fn fingerprint(value: &impl Serialize) -> Result<String, Error> {
    Ok(hex::encode(Sha256::digest(
        sonic_rs::to_vec(value).map_err(err)?,
    )))
}
fn regular(path: &Path, directory: bool) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| if directory { m.is_dir() } else { m.is_file() })
}
fn safe_path(path: &str) -> bool {
    !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn executable(path: &Path) -> Result<PathBuf, Error> {
    if path.is_absolute() || path.components().count() > 1 {
        return fs::canonicalize(path).map_err(err);
    }
    let search = std::env::var_os("PATH").ok_or(Error::Config)?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(path);
        if is_executable_file(&candidate) {
            return fs::canonicalize(candidate).map_err(err);
        }
        #[cfg(windows)]
        if path.extension().is_none() {
            let candidate = candidate.with_extension("exe");
            if is_executable_file(&candidate) {
                return fs::canonicalize(candidate).map_err(err);
            }
        }
    }
    Err(Error::Config)
}
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn file_hash(path: &Path, cancel: &AtomicBool) -> Result<String, Error> {
    let mut file = fs::File::open(path).map_err(err)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = file.read(&mut buffer).map_err(err)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> (Cache, PathBuf, ResourceReport) {
        let directory = root.join("cache");
        let guard = crate::cache::Guard::acquire(&directory).unwrap();
        let cache = Cache {
            root: directory,
            scope: "fixture".into(),
            limit: 1024,
            cancel: Default::default(),
            _guard: guard,
        };
        let output = root.join("output");
        fs::create_dir(&output).unwrap();
        let source = output.join("00000");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("item.bin"), b"fixture").unwrap();
        let report = ResourceReport {
            source: "input".into(),
            source_sha256: "source-hash".into(),
            output_directory: "00000".into(),
            outputs: vec![super::super::OutputRecord {
                path: "item.bin".into(),
                bytes: 7,
                sha256: hex::encode(Sha256::digest(b"fixture")),
                kind: "test".into(),
                object: None,
            }],
            ..Default::default()
        };
        (cache, output, report)
    }
    #[test]
    fn malformed_manifest_unsafe_paths_budget_and_cancel_never_publish() {
        let root = tempfile::tempdir().unwrap();
        let (mut cache, output, report) = fixture(root.path());
        let id = "a".repeat(64);
        cache.store(&id, &output.join("00000"), &report).unwrap();
        let metadata = cache.root.join(&id).join("entry.json");
        let original = fs::read(&metadata).unwrap();
        let mut entry: Entry = sonic_rs::from_slice(&original).unwrap();
        entry.report.objects = 999;
        fs::write(&metadata, sonic_rs::to_vec(&entry).unwrap()).unwrap();
        assert!(cache.restore(&id, &output, 1).unwrap().is_none());
        for path in [
            "../outside",
            "/outside",
            "C:/outside",
            "dir/../outside",
            "dir\\outside",
            "",
        ] {
            entry.report.outputs[0].path = path.into();
            entry.report_sha256 = fingerprint(&entry.report).unwrap();
            fs::write(&metadata, sonic_rs::to_vec(&entry).unwrap()).unwrap();
            assert!(cache.restore(&id, &output, 1).unwrap().is_none(), "{path}");
        }
        fs::write(&metadata, &original).unwrap();
        cache.limit = 1;
        assert!(cache.restore(&id, &output, 1).unwrap().is_none());
        cache.limit = 1024;
        cache.cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            cache.restore(&id, &output, 1),
            Err(Error::Cancelled)
        ));
        assert!(!output.join("00001").exists());
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        assert_eq!(fs::read(&metadata).unwrap(), original);
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_cached_payload_is_a_miss_without_following_or_deleting_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let (cache, output, report) = fixture(root.path());
        let id = "a".repeat(64);
        cache.store(&id, &output.join("00000"), &report).unwrap();
        let payload = cache.root.join(&id).join("data/item.bin");
        fs::remove_file(&payload).unwrap();
        symlink(output.join("00000/item.bin"), &payload).unwrap();
        assert!(cache.restore(&id, &output, 1).unwrap().is_none());
        cache.store(&id, &output.join("00000"), &report).unwrap();
        assert!(!fs::symlink_metadata(payload)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(output.join("00000/item.bin")).unwrap(), b"fixture");
    }
}
