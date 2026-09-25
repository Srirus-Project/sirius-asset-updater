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
        Arc, Mutex,
    },
};

use std::time::SystemTime;

const MAX_REPORT: u64 = 16 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    schema: u8,
    identity: String,
    report_sha256: String,
    report: ResourceReport,
}
#[derive(Default)]
struct Budget {
    entries: BTreeMap<String, (u64, SystemTime)>,
    bytes: u64,
}
pub(super) struct Cache {
    root: PathBuf,
    scope: String,
    limit: u64,
    max_bytes: Option<u64>,
    max_entries: Option<usize>,
    budget: Mutex<Budget>,
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
        let ffmpeg = if config.raw_only() {
            None
        } else {
            Some(executable(&config.ffmpeg)?)
        };
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
            (
                &config.selection,
                &config.read_kinds,
                &config.raw_bundles,
                &config.cri,
            ),
            &config.image,
            (
                &config.audio,
                config.video,
                config.media_backend,
                config.media_backend.identity(),
            ),
            key,
            split,
            &config.cache_revision,
            file_hash(&binary, &config.cancel)?,
            ffmpeg
                .as_ref()
                .map(|p| file_hash(p, &config.cancel))
                .transpose()?,
        ))?;
        let cache = Self {
            root,
            scope,
            limit: config.max_resource_output_bytes,
            max_bytes: config.cache_max_bytes,
            max_entries: config.cache_max_entries,
            budget: Mutex::new(Budget::default()),
            cancel: config.cancel.clone(),
            _guard: guard,
        };
        cache.scan_and_prune()?;
        Ok(Some(cache))
    }
    fn scan_and_prune(&self) -> Result<(), Error> {
        let mut budget = self.budget.lock().map_err(|_| Error::Verification)?;
        *budget = Budget::default();
        for entry in fs::read_dir(&self.root).map_err(err)? {
            self.check_cancel()?;
            let entry = entry.map_err(err)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".pending-export-") {
                remove_managed(&entry.path())?;
            } else if digest(&name) {
                let bytes = tree_size(&entry.path(), &self.cancel)?;
                let accessed = fs::symlink_metadata(entry.path().join("entry.json"))
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                budget.bytes = budget.bytes.checked_add(bytes).ok_or(Error::Size)?;
                budget.entries.insert(name, (bytes, accessed));
            }
        }
        self.make_room(&mut budget, None, 0, 0)
    }
    fn make_room(
        &self,
        budget: &mut Budget,
        replacing: Option<&str>,
        bytes: u64,
        count: usize,
    ) -> Result<(), Error> {
        loop {
            let old = replacing.and_then(|id| budget.entries.get(id));
            let desired_bytes = budget
                .bytes
                .checked_sub(old.map_or(0, |v| v.0))
                .and_then(|n| n.checked_add(bytes))
                .ok_or(Error::Size)?;
            let desired_count = budget.entries.len() - usize::from(old.is_some()) + count;
            if self.max_bytes.is_none_or(|limit| desired_bytes <= limit)
                && self.max_entries.is_none_or(|limit| desired_count <= limit)
            {
                return Ok(());
            }
            self.check_cancel()?;
            let Some(id) = budget
                .entries
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != replacing)
                .min_by_key(|(id, (_, time))| (*time, *id))
                .map(|(id, _)| id.clone())
            else {
                return Err(Error::Size);
            };
            remove_managed(&self.root.join(&id))?;
            let removed = budget.entries.remove(&id).ok_or(Error::Verification)?;
            budget.bytes -= removed.0;
        }
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
        // Cache copies share this lock with eviction; decoder work stays outside it.
        let mut budget = self.budget.lock().map_err(|_| Error::Verification)?;
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
        let accessed = SystemTime::now();
        fs::OpenOptions::new()
            .write(true)
            .open(source.join("entry.json"))
            .map_err(err)?
            .set_modified(accessed)
            .map_err(err)?;
        if let Some(entry) = budget.entries.get_mut(id) {
            entry.1 = accessed;
        }
        Ok(Some(report))
    }
    fn store(&self, id: &str, source: &Path, report: &ResourceReport) -> Result<(), Error> {
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
        let size = report
            .outputs
            .iter()
            .try_fold(bytes.len() as u64, |n, item| {
                n.checked_add(item.bytes).ok_or(Error::Size)
            })?;
        if self.max_bytes.is_some_and(|limit| size > limit) {
            return Ok(());
        }
        let mut budget = self.budget.lock().map_err(|_| Error::Verification)?;
        self.make_room(&mut budget, Some(id), size, 1)?;
        let work = tempfile::Builder::new()
            .prefix(".pending-export-")
            .tempdir_in(&self.root)
            .map_err(err)?;
        let data = work.path().join("data");
        fs::create_dir(&data).map_err(err)?;
        if !self.copy_outputs(source, &data, report)? {
            return Err(Error::Verification);
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
        fs::rename(work.path(), target).map_err(err)?;
        if let Some(old) = budget.entries.insert(id.into(), (size, SystemTime::now())) {
            budget.bytes -= old.0;
        }
        budget.bytes = budget.bytes.checked_add(size).ok_or(Error::Size)?;
        Ok(())
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
fn remove_managed(path: &Path) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path).map_err(err)?;
    if meta.is_dir() {
        fs::remove_dir_all(path).map_err(err)
    } else {
        fs::remove_file(path).map_err(err)
    }
}
fn tree_size(root: &Path, cancel: &AtomicBool) -> Result<u64, Error> {
    let mut pending = vec![root.to_owned()];
    let mut bytes = 0u64;
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let meta = fs::symlink_metadata(&path).map_err(err)?;
        if meta.is_dir() {
            for entry in fs::read_dir(path).map_err(err)? {
                pending.push(entry.map_err(err)?.path());
            }
        } else {
            bytes = bytes.checked_add(meta.len()).ok_or(Error::Size)?;
        }
    }
    Ok(bytes)
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
            max_bytes: None,
            max_entries: None,
            budget: Mutex::new(Budget::default()),
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
    #[test]
    fn cache_budget_evicts_lru_preserves_outputs_and_recovers_inventory() {
        let root = tempfile::tempdir().unwrap();
        let (mut cache, output, report) = fixture(root.path());
        cache.max_entries = Some(2);
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        cache.store(&a, &output.join("00000"), &report).unwrap();
        cache.store(&b, &output.join("00000"), &report).unwrap();
        // Durable mtime is the restart ordering; force B older than the subsequent A hit.
        fs::OpenOptions::new()
            .write(true)
            .open(cache.root.join(&b).join("entry.json"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        cache.scan_and_prune().unwrap();
        assert!(cache.restore(&a, &output, 1).unwrap().is_some());
        cache.store(&c, &output.join("00000"), &report).unwrap();
        assert!(cache.root.join(&a).exists());
        assert!(!cache.root.join(&b).exists());
        assert!(cache.root.join(&c).exists());
        assert_eq!(fs::read(output.join("00001/item.bin")).unwrap(), b"fixture");
        let one_entry = cache.budget.lock().unwrap().entries[&a].0;
        fs::create_dir(cache.root.join(".pending-export-interrupted")).unwrap();
        fs::write(
            cache.root.join(".pending-export-interrupted/partial"),
            b"unused",
        )
        .unwrap();
        fs::write(cache.root.join("operator-note.txt"), b"untouched").unwrap();
        cache.max_bytes = Some(one_entry);
        cache.scan_and_prune().unwrap();
        assert_eq!(cache.budget.lock().unwrap().entries.len(), 1);
        assert_eq!(cache.budget.lock().unwrap().bytes, one_entry);
        assert!(!cache.root.join(".pending-export-interrupted").exists());
        assert_eq!(
            fs::read(cache.root.join("operator-note.txt")).unwrap(),
            b"untouched"
        );
        cache.max_bytes = Some(1);
        cache.scan_and_prune().unwrap();
        cache.store(&a, &output.join("00000"), &report).unwrap();
        assert!(
            cache.budget.lock().unwrap().entries.is_empty(),
            "oversized output stays exported but bypasses cache"
        );
        assert_eq!(fs::read(output.join("00000/item.bin")).unwrap(), b"fixture");
    }
    #[test]
    fn concurrent_cache_inserts_obey_one_shared_budget() {
        let root = tempfile::tempdir().unwrap();
        let (mut cache, output, report) = fixture(root.path());
        cache.max_entries = Some(2);
        std::thread::scope(|scope| {
            for n in 0..12 {
                let cache = &cache;
                let output = &output;
                let report = &report;
                scope.spawn(move || {
                    cache
                        .store(&format!("{n:064x}"), &output.join("00000"), report)
                        .unwrap()
                });
            }
        });
        let budget = cache.budget.lock().unwrap();
        assert_eq!(budget.entries.len(), 2);
        assert_eq!(
            budget.bytes,
            tree_size(&cache.root, &cache.cancel).unwrap()
                - fs::metadata(cache.root.join(".lock"))
                    .map(|m| m.len())
                    .unwrap_or(0)
        );
    }
    #[cfg(unix)]
    #[test]
    fn cache_pruning_never_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let (mut cache, output, report) = fixture(root.path());
        let id = "a".repeat(64);
        cache.store(&id, &output.join("00000"), &report).unwrap();
        std::os::unix::fs::symlink(&output, cache.root.join(&id).join("external")).unwrap();
        cache.max_bytes = Some(1);
        cache.scan_and_prune().unwrap();
        assert!(!cache.root.join(&id).exists());
        assert_eq!(fs::read(output.join("00000/item.bin")).unwrap(), b"fixture");
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
