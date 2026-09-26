//! Offline, per-resource export. A successful download is not an export receipt.
#[path = "export_cache.rs"]
mod cache;
use crate::{assets::Provider, Error, Receipt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

type CpuPermits<'a> = (
    Option<crate::media_gate::Permit<'a>>,
    Option<crate::media_gate::Permit<'a>>,
);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    pub input: PathBuf,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub selection: crate::export_options::Selection,
    #[serde(default)]
    pub read_kinds: crate::read_policy::Policy,
    #[serde(default)]
    pub cri: crate::export_options::CriExport,
    #[serde(default)]
    pub raw_bundles: Option<crate::raw_bundles::Config>,
    #[serde(default)]
    pub image: crate::export_options::ImageFormats,
    #[serde(default)]
    pub audio: crate::export_options::AudioFormats,
    #[serde(default)]
    pub video: crate::export_options::VideoExport,
    #[serde(default)]
    pub media_backend: crate::media_backend::Backend,
    pub output: PathBuf,
    #[serde(default)]
    pub retain_outputs: bool,
    #[serde(default)]
    pub cache_directory: Option<PathBuf>,
    #[serde(default)]
    pub cache_revision: String,
    #[serde(default)]
    pub cache_max_bytes: Option<u64>,
    #[serde(default)]
    pub cache_max_entries: Option<usize>,
    #[serde(default)]
    pub cri_key_env: String,
    #[serde(default)]
    pub split_acb_xor_env: Option<String>,
    #[serde(default = "default_workers")]
    pub concurrency: usize,
    #[serde(default)]
    pub cpu: crate::cpu_policy::Config,
    #[serde(skip, default = "crate::cpu_policy::available_cpus")]
    detected_cpus: usize,
    #[serde(skip)]
    cpu_gate: crate::media_gate::Gate,
    #[serde(skip)]
    service_cpu_gate: Option<(std::sync::Arc<crate::media_gate::Gate>, usize)>,
    #[serde(default)]
    pub stage_limits: crate::stage_limits::Config,
    #[serde(skip)]
    stage_gates: crate::stage_limits::Gates,
    #[serde(default)]
    pub max_in_flight_bundle_bytes: u64,
    #[serde(skip)]
    local_resource_budget: std::sync::OnceLock<crate::resource_budget::Budget>,
    #[serde(skip)]
    service_resource_budget: Option<std::sync::Arc<crate::resource_budget::Budget>>,
    #[serde(default = "default_media_concurrency")]
    pub media_concurrency: usize,
    #[serde(skip)]
    media_gate: std::sync::Arc<crate::media_gate::Gate>,
    #[serde(skip)]
    service_media_gate: Option<(std::sync::Arc<crate::media_gate::Gate>, usize)>,
    #[serde(default = "media_timeout")]
    pub media_timeout_seconds: u64,
    #[serde(default)]
    pub media_retry: crate::export_options::MediaRetry,
    #[serde(skip)]
    media_retries: std::sync::atomic::AtomicUsize,
    #[serde(skip)]
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[serde(skip)]
    ffi_conversions: std::sync::atomic::AtomicUsize,
    #[serde(skip)]
    media_fallbacks: std::sync::atomic::AtomicUsize,
    #[serde(default)]
    pub ffmpeg: PathBuf,
    #[serde(default = "max_output")]
    pub max_resource_output_bytes: u64,
}
fn default_media_concurrency() -> usize {
    2
}
fn media_timeout() -> u64 {
    120
}
fn default_workers() -> usize {
    4
}
fn max_output() -> u64 {
    2 * 1024 * 1024 * 1024
}
#[derive(Default, Deserialize, Serialize)]
pub struct ExportSummary {
    #[serde(default)]
    pub ffi_conversions: usize,
    #[serde(default)]
    pub media_fallbacks: usize,
    #[serde(default)]
    pub media_retries: usize,
    pub full_catalog: bool,
    #[serde(default)]
    pub full_export: bool,
    #[serde(default)]
    pub selection: crate::export_options::Selection,
    #[serde(default)]
    pub read_kinds: crate::read_policy::Policy,
    #[serde(default)]
    pub cri: crate::export_options::CriExport,
    #[serde(default)]
    pub raw_bundles: Option<crate::raw_bundles::Config>,
    #[serde(default)]
    pub image: crate::export_options::ImageFormats,
    #[serde(default)]
    pub audio: crate::export_options::AudioFormats,
    #[serde(default)]
    pub video: crate::export_options::VideoExport,
    #[serde(default)]
    pub media_backend: crate::media_backend::Backend,
    #[serde(default)]
    pub selected_unity_objects: usize,
    #[serde(default)]
    pub skipped_unity_objects: usize,
    pub schema_version: u8,
    pub region: crate::region::Region,
    pub platform: String,
    pub complete: bool,
    pub input_files: usize,
    pub catalog_files: usize,
    pub unity_objects: usize,
    pub catalog_sha256: String,
    #[serde(default)]
    pub cache_hits: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub output_files: usize,
    pub output_bytes: u64,
    pub payloads: BTreeMap<String, usize>,
    pub retained: bool,
}
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct OutputRecord {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) object: Option<ObjectIdentity>,
}
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ObjectIdentity {
    pub(crate) source_file: String,
    pub(crate) path_id: i64,
    pub(crate) class_id: i32,
    pub(crate) name: Option<String>,
    pub(crate) container: Option<String>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct ResourceReport {
    #[serde(default)]
    pub(crate) cache_hit: bool,
    pub(crate) source: String,
    pub(crate) output_directory: String,
    pub(crate) source_sha256: String,
    pub(crate) objects: usize,
    pub(crate) selected_objects: usize,
    pub(crate) skipped_objects: usize,
    pub(crate) outputs: Vec<OutputRecord>,
    pub(crate) errors: Vec<String>,
}
fn err(e: impl std::fmt::Display) -> Error {
    Error::Export(e.to_string())
}
/// Outcome of one FFmpeg attempt: only `Transient` failures are eligible for `media_retry`.
enum MediaFailure {
    Transient(Error),
    Final(Error),
}
/// Spawn errors the original updater retried (`sekai-asset-pipeline/src/media.rs:517-535`).
fn transient_spawn_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    #[cfg(unix)]
    let executable_busy = error.raw_os_error() == Some(libc::ETXTBSY);
    #[cfg(not(unix))]
    let executable_busy = false;
    executable_busy
        || matches!(
            error.kind(),
            ErrorKind::Interrupted
                | ErrorKind::TimedOut
                | ErrorKind::WouldBlock
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionRefused
        )
}
/// Exit status or diagnostic markers the original treated as transient
/// (`sekai-asset-pipeline/src/media.rs:537-555`); deterministic decode/codec errors fail at once.
fn transient_media_failure(status: &str, diagnostic: &str) -> bool {
    const MARKERS: &[&str] = &[
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection aborted",
        "temporarily unavailable",
        "broken pipe",
        "i/o error",
        "input/output error",
        "signal",
        "killed",
    ];
    let haystack = format!("{status} {diagnostic}").to_lowercase();
    MARKERS.iter().any(|marker| haystack.contains(marker))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    fs::write(path, sonic_rs::to_vec_pretty(value).map_err(err)?).map_err(err)
}
impl ExportConfig {
    /// Whether the CRI key a decoding export reads at start is present and parseable.
    /// Offline: reads only this process's environment and never returns the value.
    pub(crate) fn secrets_ready(&self) -> bool {
        self.raw_only() || std::env::var(&self.cri_key_env).is_ok_and(|v| v.parse::<u64>().is_ok())
    }
    fn raw_only(&self) -> bool {
        self.raw_bundles
            .as_ref()
            .is_some_and(|r| r.mode == crate::raw_bundles::Mode::Only)
    }
    fn effective_stage_limits(&self) -> Result<crate::stage_limits::Config, Error> {
        self.stage_limits.effective(&self.cpu, self.detected_cpus)
    }
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(log) = &self.logging {
            log.validate().map_err(|_| Error::Config)?;
        }
        if !self.raw_only() && (self.cri_key_env.is_empty() || self.ffmpeg.as_os_str().is_empty()) {
            return Err(Error::Config);
        }
        self.cpu.validate()?;
        self.stage_limits.validate()?;
        self.selection.validate()?;
        self.read_kinds.validate()?;
        self.media_backend.validate()?;
        self.media_retry.validate()?;
        self.image.validate()?;
        if let Some(raw) = &self.raw_bundles {
            raw.validate()?;
        }
        if self.cache_directory.is_some() && !self.retain_outputs
            || self
                .cache_directory
                .as_ref()
                .is_some_and(|p| p.as_os_str().is_empty())
            || self.cache_max_bytes == Some(0)
            || self
                .cache_max_entries
                .is_some_and(|n| n == 0 || n > 1_000_000)
            || (self.cache_directory.is_none()
                && (self.cache_max_bytes.is_some() || self.cache_max_entries.is_some()))
            || self.cache_revision.len() > 256
            || (self.cache_directory.is_none() && !self.cache_revision.is_empty())
        {
            return Err(Error::Config);
        }
        if !(1..=3600).contains(&self.media_timeout_seconds)
            || !(1..=64).contains(&self.concurrency)
            || !(1..=4).contains(&self.media_concurrency)
            || self.max_resource_output_bytes == 0
            || self.max_resource_output_bytes > 16 * 1024 * 1024 * 1024
        {
            return Err(Error::Config);
        }
        Ok(())
    }
    pub async fn run(self) -> Result<ExportSummary, Error> {
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let work = self.run_controlled(receiver);
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(err)?;
                let _ = stop.send(true);
                work.await
            }
        }
    }
    /// The caller must await this future after requesting cancellation. The exporter
    /// joins its blocking workers (and their media subprocesses) before returning.
    pub async fn run_controlled(
        self,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ExportSummary, Error> {
        self.validate()?;
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
        let key = if self.raw_only() {
            0
        } else {
            std::env::var(&self.cri_key_env)
                .map_err(|_| Error::Secret)?
                .parse::<u64>()
                .map_err(|_| Error::Secret)?
        };
        let input = fs::canonicalize(&self.input).map_err(err)?;
        let verified = tokio::select! {
            result = crate::verify::verify(&input) => result?,
            _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
        };
        if verified.asset_files_verified != verified.planned_remote_files
            || verified.asset_files_verified == 0
        {
            return Err(Error::Verification);
        }
        let receipt: Receipt =
            sonic_rs::from_slice(&fs::read(input.join("receipt.json")).map_err(err)?)
                .map_err(err)?;
        if !self.raw_only()
            && !Command::new(&self.ffmpeg)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(err)?
                .success()
        {
            return Err(Error::Config);
        }
        fs::create_dir(&self.output).map_err(err)?;
        let root = fs::canonicalize(&self.output).map_err(err)?;
        if root.starts_with(&input) {
            return Err(Error::Config);
        }
        let cancel = self.cancel.clone();
        let mut job = tokio::task::spawn_blocking(move || self.execute(input, root, receipt, key));
        tokio::select! {
            result = &mut job => result.map_err(err)?,
            _ = crate::service::cancelled(&mut stop) => {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = job.await;
                Err(Error::Cancelled)
            }
        }
    }
    fn execute(
        &self,
        input: PathBuf,
        root: PathBuf,
        receipt: Receipt,
        key: u64,
    ) -> Result<ExportSummary, Error> {
        let catalog = crate::catalog::Catalog::parse(
            &fs::read(input.join("catalog_main.bin")).map_err(err)?,
        )?;
        let plan = catalog.plan(
            receipt
                .catalog_url
                .strip_suffix("/catalog_main.bin")
                .ok_or(Error::Verification)?,
        )?;
        let by_id: BTreeMap<_, _> = plan
            .assets
            .iter()
            .filter(|a| a.provider != Provider::Cri)
            .flat_map(|a| {
                a.locations
                    .iter()
                    .map(move |id| (*id, a.relative_path.clone()))
            })
            .collect();
        let mut dependencies = BTreeMap::<String, std::collections::BTreeSet<String>>::new();
        for location in &catalog.locations {
            if let Some(owner) = location.dependencies.first().and_then(|id| by_id.get(id)) {
                let set = dependencies.entry(owner.clone()).or_default();
                for id in &location.dependencies {
                    if let Some(path) = by_id.get(id) {
                        if path != owner {
                            set.insert(path.clone());
                        }
                    }
                }
            }
        }
        let cache = cache::Cache::open(self, &input, &root, &receipt.snapshot, key)?;
        let update = receipt.update.ok_or(Error::Verification)?;
        let hashes: BTreeMap<_, _> = update
            .assets
            .iter()
            .map(|a| (a.relative_path.clone(), a.stored_sha256.clone()))
            .collect();
        let mut cache_ids = BTreeMap::new();
        if let Some(cache) = &cache {
            for asset in &update.assets {
                let mut dep_hashes = BTreeMap::new();
                if let Some(paths) = dependencies.get(&asset.relative_path) {
                    for path in paths {
                        dep_hashes.insert(
                            path.clone(),
                            hashes.get(path).ok_or(Error::Verification)?.clone(),
                        );
                    }
                }
                cache_ids.insert(
                    asset.relative_path.clone(),
                    cache.identity(asset, &dep_hashes)?,
                );
            }
        }
        let complete_selection =
            catalog.select(&update.selection)?.locations.len() == catalog.locations.len();
        let mut assets = update.assets;
        let catalog_files = plan.assets.len();
        assets.retain(|a| self.selection.provider(a.provider));
        if !self.paths.is_empty() {
            let selected: std::collections::BTreeSet<_> = self.paths.iter().collect();
            assets.retain(|a| selected.contains(&a.relative_path));
            if assets.len() != selected.len() {
                return Err(Error::AssetPath);
            }
        }
        if self.raw_only() {
            assets.retain(|a| {
                self.raw_bundles
                    .as_ref()
                    .is_some_and(|r| r.matches(a.provider, &a.relative_path))
            });
        }
        if assets.is_empty() {
            return Err(Error::Selection);
        }
        let mut summary = ExportSummary {
            raw_bundles: self.raw_bundles.clone(),
            full_export: !self.raw_only()
                && complete_selection
                && assets.len() == catalog_files
                && self.selection.full()
                && self.read_kinds.is_native()
                && self.cri.full(),
            selection: self.selection.clone(),
            read_kinds: self.read_kinds.clone(),
            cri: self.cri.clone(),
            image: self.image.clone(),
            audio: self.audio.clone(),
            video: self.video,
            media_backend: self.media_backend,
            full_catalog: complete_selection && assets.len() == catalog_files,
            schema_version: 4,
            region: receipt.snapshot.region.unwrap_or_default(),
            platform: receipt.snapshot.platform.clone(),
            input_files: assets.len(),
            catalog_files,
            catalog_sha256: receipt.sha256.clone(),
            retained: self.retain_outputs,
            ..ExportSummary::default()
        };
        let mut journal = fs::File::create(root.join("resources.jsonl")).map_err(err)?;
        let next = std::sync::atomic::AtomicUsize::new(0);
        let workers = self
            .cpu
            .workers_for_cpus(self.concurrency, self.detected_cpus)?
            .min(assets.len());
        let stages = self.effective_stage_limits()?;
        tracing::info!(
            workers,
            stage_auto_tune = stages.auto_tune,
            acb = ?stages.acb, usm = ?stages.usm, hca = ?stages.hca, image = ?stages.image,
            audio_encode = ?stages.audio_encode, video_encode = ?stages.video_encode,
            configured_workers = self.concurrency,
            auto_tune = self.cpu.auto_tune,
            "export resource worker pool"
        );
        std::thread::scope(|scope| -> Result<(), Error> {
            let (send, recv) = std::sync::mpsc::sync_channel(workers);
            for _ in 0..workers {
                let send = send.clone();
                let assets = &assets;
                let next = &next;
                let input = &input;
                let dependencies = &dependencies;
                let root = &root;
                let cache = &cache;
                let cache_ids = &cache_ids;
                scope.spawn(move || loop {
                    if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(asset) = assets.get(i) else {
                        break;
                    };
                    let report =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let decode = || {
                                self.process_resource(
                                    input,
                                    root,
                                    asset,
                                    i,
                                    key,
                                    dependencies.get(&asset.relative_path),
                                )
                            };
                            if let Some(cache) = cache {
                                cache.process(&cache_ids[&asset.relative_path], root, i, decode)
                            } else {
                                decode()
                            }
                        })) {
                            Ok(Ok(report)) => report,
                            result => ResourceReport {
                                source: asset.relative_path.clone(),
                                source_sha256: asset.stored_sha256.clone(),
                                output_directory: format!("{i:05}"),
                                errors: vec![match result {
                                    Ok(Err(error)) => error.to_string(),
                                    _ => "decoder worker panicked".into(),
                                }],
                                ..ResourceReport::default()
                            },
                        };
                    if send.send(report).is_err() {
                        break;
                    }
                });
            }
            drop(send);
            for report in recv {
                if report.errors.is_empty() {
                    summary.succeeded += 1;
                    summary.cache_hits += usize::from(report.cache_hit);
                } else {
                    summary.failed += 1;
                }
                summary.unity_objects += report.objects;
                summary.selected_unity_objects += report.selected_objects;
                summary.skipped_unity_objects += report.skipped_objects;
                for output in &report.outputs {
                    summary.output_files += 1;
                    summary.output_bytes += output.bytes;
                    *summary.payloads.entry(output.kind.clone()).or_default() += 1;
                }
                journal
                    .write_all(&sonic_rs::to_vec(&report).map_err(err)?)
                    .map_err(err)?;
                journal.write_all(b"\n").map_err(err)?;
                journal.flush().map_err(err)?;
                let completed = summary.succeeded + summary.failed;
                if completed.is_multiple_of(25) || !report.errors.is_empty() {
                    tracing::info!(
                        stage = "export",
                        completed,
                        total = assets.len(),
                        failed = summary.failed,
                        bytes = summary.output_bytes,
                        "Resource export progress"
                    );
                }
                summary.ffi_conversions = self
                    .ffi_conversions
                    .load(std::sync::atomic::Ordering::Relaxed);
                summary.media_fallbacks = self
                    .media_fallbacks
                    .load(std::sync::atomic::Ordering::Relaxed);
                summary.media_retries = self
                    .media_retries
                    .load(std::sync::atomic::Ordering::Relaxed);
                write_json(&root.join("summary.json"), &summary)?;
            }
            Ok(())
        })?;
        summary.complete = summary.failed == 0
            && summary.succeeded == summary.input_files
            && summary.output_files > 0;
        write_json(&root.join("summary.json"), &summary)?;
        Ok(summary)
    }
    fn process_resource(
        &self,
        input: &Path,
        root: &Path,
        asset: &crate::update::AssetReceipt,
        i: usize,
        key: u64,
        dependencies: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<ResourceReport, Error> {
        let mut weight = asset.bytes;
        if let Some(dependencies) = dependencies.filter(|_| {
            self.max_in_flight_bundle_bytes > 0 || self.service_resource_budget.is_some()
        }) {
            for dependency in dependencies {
                let metadata =
                    fs::symlink_metadata(input.join("assets").join(dependency)).map_err(err)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(Error::Verification);
                }
                weight = weight.checked_add(metadata.len()).ok_or(Error::Size)?;
            }
        }
        let _local = if self.max_in_flight_bundle_bytes > 0 {
            Some(
                self.local_resource_budget
                    .get_or_init(|| {
                        crate::resource_budget::Budget::new(self.max_in_flight_bundle_bytes)
                    })
                    .acquire(weight, &self.cancel)?,
            )
        } else {
            None
        };
        let _shared = self
            .service_resource_budget
            .as_ref()
            .map(|budget| budget.acquire(weight, &self.cancel))
            .transpose()?;
        let work = tempfile::Builder::new()
            .prefix(".export-")
            .tempdir_in(root)
            .map_err(err)?;
        let path = input.join("assets").join(&asset.relative_path);
        let mut report = ResourceReport {
            source: asset.relative_path.clone(),
            output_directory: format!("{i:05}"),
            source_sha256: asset.stored_sha256.clone(),
            ..ResourceReport::default()
        };
        let result = (|| -> Result<(), Error> {
            if let Some(raw) = &self.raw_bundles {
                if raw.matches(asset.provider, &asset.relative_path) {
                    self.copy_raw_bundle(&path, work.path(), asset, raw, &mut report)?;
                }
            }
            if self.raw_only() {
                return Ok(());
            }
            match asset.provider {
                Provider::EncryptedBundle | Provider::UnityBundle => self.unity(
                    &path,
                    work.path(),
                    key,
                    &mut report,
                    dependencies,
                    &input.join("assets"),
                ),
                Provider::Cri => self.cri(&path, work.path(), key, &mut report),
            }
        })();
        if let Err(error) = result {
            report.errors.push(error.to_string());
        }
        if report.errors.is_empty() && self.retain_outputs {
            fs::rename(work.path(), root.join(format!("{i:05}"))).map_err(err)?;
        }
        Ok(report)
    }
    fn copy_raw_bundle(
        &self,
        source: &Path,
        root: &Path,
        asset: &crate::update::AssetReceipt,
        raw: &crate::raw_bundles::Config,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        use std::io::Read;
        let metadata = fs::symlink_metadata(source).map_err(err)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Verification);
        }
        let prior: u64 = report.outputs.iter().map(|r| r.bytes).sum();
        if prior.saturating_add(metadata.len()) > self.max_resource_output_bytes {
            return Err(Error::Size);
        }
        let relative = raw.output_path(&asset.relative_path)?;
        let target = root.join(&relative);
        fs::create_dir_all(target.parent().ok_or(Error::AssetPath)?).map_err(err)?;
        let mut input = fs::File::open(source).map_err(err)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(err)?;
        let mut bytes = 0u64;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = input.read(&mut buffer).map_err(err)?;
            if n == 0 {
                break;
            }
            bytes = bytes.checked_add(n as u64).ok_or(Error::Size)?;
            if bytes > metadata.len()
                || prior.saturating_add(bytes) > self.max_resource_output_bytes
            {
                return Err(Error::Size);
            }
            digest.update(&buffer[..n]);
            output.write_all(&buffer[..n]).map_err(err)?;
        }
        let sha256 = hex::encode(digest.finalize());
        if bytes != metadata.len() || sha256 != asset.stored_sha256 {
            return Err(Error::Verification);
        }
        report.outputs.push(OutputRecord {
            path: relative,
            kind: "raw_bundle".into(),
            bytes,
            sha256,
            object: None,
        });
        Ok(())
    }
    fn unity(
        &self,
        path: &Path,
        output: &Path,
        key: u64,
        report: &mut ResourceReport,
        dependencies: Option<&std::collections::BTreeSet<String>>,
        asset_root: &Path,
    ) -> Result<(), Error> {
        use unity_rs_core::{
            image_export::ImageRowOrder, sprite::SpriteReadLimits, studio::Studio,
            texture::TextureReadLimits,
        };
        let mut studio = {
            let _cpu = self.acquire_cpu(self.cpu_deadline())?;
            Studio::open(path).map_err(err)?
        };
        let own_files: std::collections::BTreeSet<_> =
            studio.files().map(|f| f.path().to_string()).collect();
        // Cross-bundle atlas and streamed media references use catalog dependency closure.
        if studio.objects().any(|object| {
            matches!(
                object.class_id(),
                213 | unity_rs_core::simple_assets::AUDIO_CLIP_CLASS_ID
                    | unity_rs_core::simple_assets::VIDEO_CLIP_CLASS_ID
                    | unity_rs_core::texture_array::TEXTURE_2D_ARRAY_CLASS_ID
            ) && self.selection.class(object.class_id())
                && self
                    .read_kinds
                    .for_class(object.class_id())
                    .is_ok_and(|kind| {
                        !matches!(
                            kind,
                            crate::read_policy::Kind::ObjectRaw
                                | crate::read_policy::Kind::TypetreeJson
                        )
                    })
        }) {
            if let Some(dependencies) = dependencies.filter(|d| !d.is_empty()) {
                let mut regions = vec![(
                    path.to_string_lossy().into_owned(),
                    unity_rs_core::source::Region::from_file(path).map_err(err)?,
                )];
                let base = asset_root;
                let mut total = fs::metadata(path).map_err(err)?.len();
                for dependency in dependencies {
                    let p = base.join(dependency);
                    total = total
                        .checked_add(fs::metadata(&p).map_err(err)?.len())
                        .ok_or(Error::Size)?;
                    if total > 2 * 1024 * 1024 * 1024 {
                        return Err(Error::Size);
                    }
                    regions.push((
                        p.to_string_lossy().into_owned(),
                        unity_rs_core::source::Region::from_file(p).map_err(err)?,
                    ));
                }
                studio = {
                    let _cpu = self.acquire_cpu(self.cpu_deadline())?;
                    Studio::open_regions(regions).map_err(err)?
                };
            }
        }
        for diagnostic in studio.load_diagnostics() {
            report.errors.push(format!("load: {}", diagnostic.message));
        }
        report.objects = studio
            .objects()
            .filter(|o| own_files.contains(o.source_path()))
            .count();
        if report.objects == 0 {
            return Err(err("bundle contains no serialized objects"));
        }
        let mut embedded = 0;
        for object in studio
            .objects()
            .filter(|o| own_files.contains(o.source_path()))
        {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.selection.class(object.class_id()) {
                report.skipped_objects += 1;
                continue;
            }
            report.selected_objects += 1;
            let stem = format!("{}_{}", object.file_index(), object.path_id());
            let output_start = report.outputs.len();
            let result = (|| -> Result<(), Error> {
                use crate::read_policy::Kind;
                let read_kind = self.read_kinds.for_class(object.class_id())?;
                if matches!(read_kind, Kind::ObjectRaw | Kind::TypetreeJson) {
                    let cpu = self.acquire_cpu(self.cpu_deadline())?;
                    let limit = self.max_resource_output_bytes.min(512 * 1024 * 1024);
                    let (bytes, extension, kind) = if read_kind == Kind::ObjectRaw {
                        (
                            object.read_raw(limit).map_err(err)?,
                            "object.bin",
                            "raw_object",
                        )
                    } else {
                        (
                            object
                                .read_type_tree_json(false, limit as usize)
                                .map_err(err)?,
                            "json",
                            "typetree_json",
                        )
                    };
                    drop(cpu);
                    if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    let target = output.join(format!("{stem}.{extension}"));
                    fs::write(&target, bytes).map_err(err)?;
                    self.record(output, &target, kind, report)?;
                    if read_kind == Kind::TypetreeJson {
                        let file =
                            &studio.collection().serialized_files()[object.file_index()].file;
                        let tree = file.object_type_tree(object.object_index()).map_err(err)?;
                        if tree
                            .nodes
                            .iter()
                            .any(|node| node.type_name == "TypelessData")
                        {
                            let cpu = self.acquire_cpu(self.cpu_deadline())?;
                            let bytes = object.read_raw(limit).map_err(err)?;
                            drop(cpu);
                            let target = output.join(format!("{stem}.object.bin"));
                            fs::write(&target, bytes).map_err(err)?;
                            self.record(output, &target, "typetree_binary_source", report)?;
                        }
                    }
                    return Ok(());
                }
                use unity_rs_core::simple_assets::{
                    SimpleAssetReadLimits, AUDIO_CLIP_CLASS_ID, MOVIE_TEXTURE_CLASS_ID,
                    VIDEO_CLIP_CLASS_ID,
                };
                if matches!(
                    object.class_id(),
                    AUDIO_CLIP_CLASS_ID | VIDEO_CLIP_CLASS_ID | MOVIE_TEXTURE_CLASS_ID
                ) {
                    let cpu = self.acquire_cpu(self.cpu_deadline())?;
                    let limit = self.max_resource_output_bytes.min(512 * 1024 * 1024);
                    let limits = SimpleAssetReadLimits {
                        maximum_payload_bytes: limit,
                        ..Default::default()
                    };
                    let (payload, extension, kind) = if object.class_id() == AUDIO_CLIP_CLASS_ID {
                        let asset = object.read_audio_clip(limits).map_err(err)?;
                        (asset.payload, asset.raw_extension, "audio_raw")
                    } else {
                        let asset = if object.class_id() == VIDEO_CLIP_CLASS_ID {
                            object.read_video_clip(limits).map_err(err)?
                        } else {
                            object.read_movie_texture(limits).map_err(err)?
                        };
                        (asset.payload, asset.suggested_extension, asset.payload_kind)
                    };
                    let suffix = extension
                        .strip_prefix('.')
                        .filter(|suffix| {
                            !suffix.is_empty()
                                && suffix.len() <= 16
                                && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
                        })
                        .ok_or(Error::AssetPath)?;
                    let bytes = payload.read_to_vec(limit).map_err(err)?;
                    drop(cpu);
                    let prior: u64 = report.outputs.iter().map(|o| o.bytes).sum();
                    if prior.saturating_add(bytes.len() as u64) > self.max_resource_output_bytes {
                        return Err(Error::Size);
                    }
                    if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    let target = output.join(format!("{stem}.{suffix}"));
                    fs::write(&target, bytes).map_err(err)?;
                    self.record(output, &target, kind, report)?;
                    return Ok(());
                }
                let image_stage = if matches!(
                    object.class_id(),
                    28 | 213 | unity_rs_core::texture_array::TEXTURE_2D_ARRAY_CLASS_ID
                ) {
                    self.stage_gates.acquire(
                        &self.effective_stage_limits()?,
                        crate::stage_limits::Stage::Image,
                        &self.cancel,
                    )?
                } else {
                    None
                };
                let cpu = self.acquire_cpu(self.cpu_deadline())?;
                let limit = self.max_resource_output_bytes.min(512 * 1024 * 1024);
                if object.class_id() == unity_rs_core::texture_array::TEXTURE_2D_ARRAY_CLASS_ID {
                    use unity_rs_core::texture_array::{
                        read_texture2d_array, TextureArrayReadLimits,
                    };
                    let limits = TextureArrayReadLimits::default();
                    let file = &studio.collection().serialized_files()[object.file_index()].file;
                    let array = read_texture2d_array(
                        studio.collection(),
                        file,
                        object.object_index(),
                        limits,
                    )
                    .map_err(err)?;
                    let decoded_bytes = u64::from(array.width)
                        .checked_mul(u64::from(array.height))
                        .and_then(|n| n.checked_mul(4))
                        .and_then(|n| n.checked_mul(u64::from(array.layer_count())))
                        .ok_or(Error::Size)?;
                    if decoded_bytes > limits.maximum_output_bytes {
                        return Err(Error::Size);
                    }
                    drop(cpu);
                    for layer in 0..array.layer_count() {
                        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        let cpu = self.acquire_cpu(self.cpu_deadline())?;
                        let image = array.decode_layer_mip0_rgba8(layer, limits).map_err(err)?;
                        drop(cpu);
                        self.image_outputs(
                            &image,
                            ImageRowOrder::UnityDecoded,
                            output,
                            &format!("{stem}_layer_{layer:04}"),
                            report,
                        )?;
                    }
                    return Ok(());
                }
                if matches!(object.class_id(), 28 | 213) {
                    let image = if object.class_id() == 28 {
                        object
                            .decode_texture_mip(0, TextureReadLimits::default())
                            .map_err(err)?
                    } else {
                        object
                            .decode_sprite(
                                SpriteReadLimits::default(),
                                TextureReadLimits::default(),
                            )
                            .map_err(err)?
                    };
                    drop(cpu);
                    return self.image_outputs(
                        &image,
                        if object.class_id() == 28 {
                            ImageRowOrder::UnityDecoded
                        } else {
                            ImageRowOrder::Display
                        },
                        output,
                        &stem,
                        report,
                    );
                }
                let (data, extension, kind) = match object.class_id() {
                    114 if is_moc_object(&studio, object)? => {
                        let file =
                            &studio.collection().serialized_files()[object.file_index()].file;
                        let moc = unity_rs_core::cubism_moc::read_cubism_moc(
                            file,
                            object.object_index(),
                            unity_rs_core::cubism_moc::CubismMocReadLimits::default(),
                        )
                        .map_err(err)?;
                        (
                            moc.model_data.read_to_vec(limit).map_err(err)?,
                            "moc3",
                            "cubism_moc3",
                        )
                    }
                    114 if object.byte_size() > 4 * 1024 * 1024
                        && object.byte_size() <= 32 * 1024 * 1024 =>
                    {
                        let file =
                            &studio.collection().serialized_files()[object.file_index()].file;
                        let value = file
                            .read_type_tree_value_with_limits(
                                object.object_index(),
                                unity_rs_core::type_tree::TypeTreeReadLimits {
                                    maximum_materialized_bytes: 2 * 1024 * 1024 * 1024,
                                    ..unity_rs_core::type_tree::TypeTreeReadLimits::default()
                                },
                            )
                            .map_err(err)?;
                        let mut writer = JsonBuffer {
                            bytes: Vec::new(),
                            limit: limit as usize,
                        };
                        unity_rs_core::json::write_type_value_json(&value, &mut writer, false)
                            .map_err(err)?;
                        (writer.bytes, "json", "typetree_json")
                    }
                    49 => (
                        object.read_text_bytes(limit as usize).map_err(err)?,
                        "bytes",
                        "text_bytes",
                    ),
                    43 => {
                        match object.read_mesh_obj(unity_rs_core::mesh::MeshReadLimits::default()) {
                            Ok(bytes) => (bytes, "obj", "mesh_obj"),
                            Err(unity_rs_core::Error::Unsupported(reason))
                                if read_kind == Kind::Auto
                                    && reason.contains("Mesh has no vertices") =>
                            {
                                let bytes = object
                                    .read_type_tree_json(false, limit as usize)
                                    .map_err(err)?;
                                let value: sonic_rs::Value =
                                    sonic_rs::from_slice(&bytes).map_err(err)?;
                                if !empty_mesh(&value) {
                                    return Err(err("Mesh geometry could not be decoded"));
                                }
                                (bytes, "json", "empty_mesh_json")
                            }
                            Err(error) => return Err(err(error)),
                        }
                    }
                    48 => match object.read_shader_text(limit) {
                        Ok(bytes) => (bytes, "shader", "shader_text"),
                        Err(unity_rs_core::Error::Unsupported(_)) if read_kind == Kind::Auto => (
                            object
                                .read_type_tree_json(false, limit as usize)
                                .map_err(err)?,
                            "json",
                            "shader_typetree_json",
                        ),
                        Err(error) => return Err(err(error)),
                    },
                    128 => {
                        let font = object
                            .read_font(
                                unity_rs_core::simple_assets::SimpleAssetReadLimits::default(),
                            )
                            .map_err(err)?;
                        (
                            font.payload.read_to_vec(limit).map_err(err)?,
                            "font",
                            "font_bytes",
                        )
                    }
                    _ => (
                        object
                            .read_type_tree_json(false, limit as usize)
                            .map_err(err)?,
                        "json",
                        "typetree_json",
                    ),
                };
                let target = output.join(format!("{stem}.{extension}"));
                fs::write(&target, &data).map_err(err)?;
                self.record(output, &target, kind, report)?;
                if extension == "json" {
                    let file = &studio.collection().serialized_files()[object.file_index()].file;
                    let tree = file.object_type_tree(object.object_index()).map_err(err)?;
                    if tree.nodes.iter().any(|n| n.type_name == "TypelessData") {
                        // JSON stores object-relative Offset/Size for opaque bytes.
                        // Retain their backing object only after successful parsing.
                        let target = output.join(format!("{stem}.object.bin"));
                        fs::write(&target, object.read_raw(limit).map_err(err)?).map_err(err)?;
                        self.record(output, &target, "typetree_binary_source", report)?;
                    }
                }
                drop(cpu);
                drop(image_stage);
                if self.selection.embedded_audio
                    && object.class_id() == 114
                    && kind == "typetree_json"
                {
                    let value: sonic_rs::Value = sonic_rs::from_slice(&data).map_err(err)?;
                    if let Some(chunks) = value.get("_chunks").and_then(|v| v.as_array()) {
                        if chunks.is_empty() {
                            return Err(err("SplitAcb has no chunks"));
                        }
                        let mut joined = Vec::new();
                        for chunk in chunks {
                            let file_id = chunk
                                .get("m_FileID")
                                .and_then(|v| v.as_i64())
                                .ok_or_else(|| err("missing chunk file ID"))?;
                            let path_id = chunk
                                .get("m_PathID")
                                .and_then(|v| v.as_i64())
                                .ok_or_else(|| err("missing chunk path ID"))?;
                            let reference = unity_rs_core::serialized::ObjectReference {
                                file_id: i32::try_from(file_id).map_err(err)?,
                                path_id,
                            };
                            let target = unity_rs_core::scene::resolve_object_reference(
                                studio.collection(),
                                object.file_index(),
                                reference,
                            )
                            .map_err(err)?
                            .ok_or_else(|| err("null SplitAcb chunk"))?;
                            let chunk = target
                                .file
                                .read_text_asset(target.object_index, limit as usize)
                                .map_err(err)?;
                            if joined.len().saturating_add(chunk.script.len()) as u64 > limit {
                                return Err(Error::Size);
                            }
                            joined.extend(chunk.script);
                        }
                        if !joined.starts_with(b"@UTF") {
                            let env = self.split_acb_xor_env.as_ref().ok_or(Error::Secret)?;
                            let value = std::env::var(env).map_err(|_| Error::Secret)?;
                            let mask =
                                u8::from_str_radix(value.strip_prefix("0x").unwrap_or(&value), 16)
                                    .map_err(|_| Error::Secret)?;
                            joined.iter_mut().for_each(|b| *b ^= mask);
                        }
                        if !joined.starts_with(b"@UTF") {
                            return Err(err("SplitAcb reassembly is not ACB"));
                        }
                        let dir = output.join(format!("{stem}.acb"));
                        fs::create_dir(&dir).map_err(err)?;
                        self.acb(&joined, &dir, key, report, output)?;
                        embedded += 1;
                    } else if let Some(refs) = value
                        .get("references")
                        .and_then(|v| v.get("RefIds"))
                        .and_then(|v| v.as_array())
                    {
                        for item in refs {
                            if item
                                .get("type")
                                .and_then(|v| v.get("class"))
                                .and_then(|v| v.as_str())
                                != Some("CriSerializedBytesAssetImpl")
                            {
                                continue;
                            }
                            let array = item
                                .get("data")
                                .and_then(|v| v.get("data"))
                                .and_then(|v| v.as_array())
                                .ok_or_else(|| err("CRI inline byte array missing"))?;
                            let bytes: Vec<u8> = array
                                .iter()
                                .map(|v| {
                                    v.as_u64()
                                        .and_then(|n| u8::try_from(n).ok())
                                        .ok_or_else(|| err("invalid inline CRI byte"))
                                })
                                .collect::<Result<_, _>>()?;
                            if !bytes.starts_with(b"@UTF") {
                                return Err(err("inline CRI bytes are not ACB"));
                            }
                            let dir = output.join(format!("{stem}-{embedded}.acb"));
                            fs::create_dir(&dir).map_err(err)?;
                            self.acb(&bytes, &dir, key, report, output)?;
                            embedded += 1;
                        }
                    }
                }
                Ok(())
            })();
            for record in &mut report.outputs[output_start..] {
                record.object = Some(ObjectIdentity {
                    source_file: Path::new(object.source_path())
                        .file_name()
                        .ok_or(Error::AssetPath)?
                        .to_string_lossy()
                        .into_owned(),
                    path_id: object.path_id(),
                    class_id: object.class_id(),
                    name: object.name().map(str::to_owned),
                    container: object.container().map(str::to_owned),
                });
            }
            if let Err(error) = result {
                report.errors.push(format!(
                    "object {} class {}: {error}",
                    object.path_id(),
                    object.class_id()
                ));
            }
        }
        Ok(())
    }
    /// Decode once in the caller, then encode/write/verify one rendition at a time.
    fn image_outputs(
        &self,
        image: &unity_rs_core::texture::RgbaImage,
        order: unity_rs_core::image_export::ImageRowOrder,
        output: &Path,
        stem: &str,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        for format in self.image.iter() {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let prior: u64 = report.outputs.iter().map(|o| o.bytes).sum();
            let remaining = self.max_resource_output_bytes.saturating_sub(prior);
            if remaining == 0 {
                return Err(Error::Size);
            }
            let cpu = self.acquire_cpu(self.cpu_deadline())?;
            let data = format.encode(
                image,
                order,
                self.max_resource_output_bytes.min(512 * 1024 * 1024),
            )?;
            drop(cpu);
            if data.len() as u64 > remaining {
                return Err(Error::Size);
            }
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let target = output.join(format!("{stem}{}", format.native().extension()));
            fs::write(&target, &data).map_err(err)?;
            drop(data);
            self.record(output, &target, format.native().payload_kind(), report)?;
        }
        Ok(())
    }
    fn preserve_cri(
        &self,
        bytes: &[u8],
        output: &Path,
        root: &Path,
        extension: &str,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let prior: u64 = report.outputs.iter().map(|r| r.bytes).sum();
        if prior.saturating_add(bytes.len() as u64) > self.max_resource_output_bytes {
            return Err(Error::Size);
        }
        let path = output.join(format!("container.{extension}"));
        fs::write(&path, bytes).map_err(err)?;
        self.record(root, &path, &format!("cri_{extension}_container"), report)
    }
    fn acb(
        &self,
        bytes: &[u8],
        output: &Path,
        key: u64,
        report: &mut ResourceReport,
        root: &Path,
    ) -> Result<(), Error> {
        if self.cri.acb == crate::export_options::ContainerMode::Preserve {
            return self.preserve_cri(bytes, output, root, "acb", report);
        }
        let _stage = self.stage_gates.acquire(
            &self.effective_stage_limits()?,
            crate::stage_limits::Stage::Acb,
            &self.cancel,
        )?;
        use cridecoder::acb::{AfsArchive, TrackList, UtfTable};
        let cpu = self.acquire_cpu(self.cpu_deadline())?;
        let utf = UtfTable::new(Cursor::new(bytes)).map_err(err)?;
        let tracks = TrackList::new(&utf).map_err(err)?;
        if tracks.tracks.iter().any(|t| t.is_stream) {
            return Err(err("ACB requires external AWB"));
        }
        let embedded = utf
            .rows
            .first()
            .and_then(|r| r.get("AwbFile"))
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| err("ACB has no embedded AWB"))?;
        let mut awb = AfsArchive::new(Cursor::new(embedded)).map_err(err)?;
        let entries = awb.files.clone();
        if entries.is_empty() {
            return Err(err("ACB contains no waveforms"));
        }
        for track in &tracks.tracks {
            if !entries.iter().any(|e| e.cue_id == track.wav_id) {
                return Err(err("cue references absent waveform"));
            }
        }
        drop(cpu);
        for (i, entry) in entries.into_iter().enumerate() {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let hca_stage = self.stage_gates.acquire(
                &self.effective_stage_limits()?,
                crate::stage_limits::Stage::Hca,
                &self.cancel,
            )?;
            let cpu = self.acquire_cpu(self.cpu_deadline())?;
            let hca = awb.file_data(entry).map_err(err)?;
            let mut decoder = cridecoder::HcaDecoder::from_reader(Cursor::new(hca)).map_err(err)?;
            decoder.set_encryption_key(key, awb.subkey as u64);
            let info = decoder.info();
            let bytes = (info.block_count as u64)
                .checked_mul(1024)
                .and_then(|n| n.checked_mul(info.channel_count as u64 * 2))
                .ok_or(Error::Size)?;
            let prior: u64 = report.outputs.iter().map(|o| o.bytes).sum();
            if bytes.saturating_add(prior).saturating_add(44) > self.max_resource_output_bytes {
                return Err(Error::Size);
            }
            let target = output.join(format!("{i:05}.wav"));
            decoder
                .decode_to_wav(&mut fs::File::create(&target).map_err(err)?)
                .map_err(err)?;
            drop(cpu);
            drop(hca_stage);
            self.media_check(&target)?;
            self.record_audio(root, &target, "hca", report)?;
            let cues: Vec<_> = tracks
                .tracks
                .iter()
                .filter(|t| t.wav_id == entry.cue_id)
                .map(|c| (&c.name, c.cue_id, entry.cue_id))
                .collect();
            let target = output.join(format!("{i:05}.cues.json"));
            write_json(&target, &cues)?;
            self.record(root, &target, "cue_metadata", report)?;
        }
        Ok(())
    }
    fn cri(
        &self,
        path: &Path,
        output: &Path,
        key: u64,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        let bytes = fs::read(path).map_err(err)?;
        if bytes.starts_with(b"@UTF") {
            return self.acb(&bytes, output, key, report, output);
        }
        if !bytes.starts_with(b"CRID") {
            return Err(err("unknown CRI container"));
        }
        if self.cri.usm == crate::export_options::ContainerMode::Preserve {
            return self.preserve_cri(&bytes, output, output, "usm", report);
        }
        let split = {
            let _cpu = self.acquire_cpu(self.cpu_deadline())?;
            split_alpha_usm(&bytes)?
        };
        if let Some((color, alpha)) = split {
            for (name, data) in [("color", color), ("alpha", alpha)] {
                let directory = output.join(name);
                fs::create_dir(&directory).map_err(err)?;
                self.usm(data, &directory, key, report, output)?;
            }
            return Ok(());
        }
        self.usm(bytes, output, key, report, output)
    }
    fn usm(
        &self,
        bytes: Vec<u8>,
        output: &Path,
        key: u64,
        report: &mut ResourceReport,
        report_root: &Path,
    ) -> Result<(), Error> {
        let _stage = self.stage_gates.acquire(
            &self.effective_stage_limits()?,
            crate::stage_limits::Stage::Usm,
            &self.cancel,
        )?;
        let start = report.outputs.len();
        match self.usm_decode(
            &bytes,
            output,
            UsmKey {
                value: key,
                masked: true,
            },
            report,
            report_root,
        ) {
            Ok(()) => Ok(()),
            Err(Error::Export(reason))
                if reason.starts_with("media decode:")
                    || reason.starts_with("video frame count") =>
            {
                report.outputs.truncate(start);
                for entry in fs::read_dir(output).map_err(err)? {
                    let entry = entry.map_err(err)?;
                    if !entry.file_type().map_err(err)?.is_file() {
                        return Err(Error::Io);
                    }
                    fs::remove_file(entry.path()).map_err(err)?;
                }
                self.usm_decode(
                    &bytes,
                    output,
                    UsmKey {
                        value: key,
                        masked: false,
                    },
                    report,
                    report_root,
                )
                .map_err(|error| {
                    err(format!(
                        "masked video failed ({reason}); unmasked video failed ({error})"
                    ))
                })
            }
            Err(error) => Err(error),
        }
    }
    fn usm_decode(
        &self,
        bytes: &[u8],
        output: &Path,
        crypto: UsmKey,
        report: &mut ResourceReport,
        report_root: &Path,
    ) -> Result<(), Error> {
        let cpu = self.acquire_cpu(self.cpu_deadline())?;
        let metadata = cridecoder::usm::read_metadata(Cursor::new(bytes), b"movie").map_err(err)?;
        let metadata_json = sonic_rs::to_vec(&metadata).map_err(err)?;
        let value: sonic_rs::Value = sonic_rs::from_slice(&metadata_json).map_err(err)?;
        let mut expected_frames = None;
        let mut legacy_adx = false;
        if let Some(sections) = value.get("sections").and_then(|v| v.as_array()) {
            for section in sections {
                if let Some(rows) = section
                    .get("data")
                    .and_then(|v| v.get("rows"))
                    .and_then(|v| v.as_array())
                {
                    for row in rows {
                        if section.get("kind").and_then(|v| v.as_str()) == Some("audio_header")
                            && row.get("audio_codec").is_none()
                        {
                            legacy_adx = true;
                        }
                        if let Some(frames) = row.get("total_frames").and_then(|v| v.as_u64()) {
                            expected_frames = Some(frames);
                        }
                    }
                }
            }
        }
        let streams = cridecoder::extract_usm_to_memory(
            Cursor::new(bytes),
            b"movie",
            crypto.masked.then_some(crypto.value),
            true,
        )
        .map_err(err)?;
        drop(cpu);
        let mut video = None;
        let mut audio = None;
        for (i, stream) in streams.into_iter().enumerate() {
            if stream.data.len() as u64 > self.max_resource_output_bytes {
                return Err(Error::Size);
            }
            match stream.extension.as_str() {
                "ivf" | "m2v" => {
                    if video.is_some() {
                        return Err(err("multiple video streams are unsupported"));
                    }
                    let target = output.join(format!("{i:05}.{}", stream.extension));
                    fs::write(&target, &stream.data).map_err(err)?;
                    self.record(report_root, &target, "usm_video_stream", report)?;
                    video = Some(target);
                }
                "adx" => {
                    if audio.is_some() {
                        return Err(err("multiple audio streams are unsupported"));
                    }
                    let target = output.join(format!("{i:05}.wav"));
                    // cridecoder 0.3.5 defaults missing audio_codec to ADX,
                    // but only unmasks explicit codec 2. Older headers omit it.
                    let legacy;
                    let data = if legacy_adx && crypto.masked {
                        legacy = legacy_adx_stream(bytes, crypto.value)?;
                        &legacy
                    } else {
                        &stream.data
                    };
                    self.adx(data, &target)?;
                    audio = Some(self.record_audio(report_root, &target, "adx", report)?);
                }
                "hca" => {
                    if audio.is_some() {
                        return Err(err("multiple audio streams are unsupported"));
                    }
                    let target = output.join(format!("{i:05}.wav"));
                    let hca_stage = self.stage_gates.acquire(
                        &self.effective_stage_limits()?,
                        crate::stage_limits::Stage::Hca,
                        &self.cancel,
                    )?;
                    let cpu = self.acquire_cpu(self.cpu_deadline())?;
                    let mut decoder = cridecoder::HcaDecoder::from_reader(Cursor::new(stream.data))
                        .map_err(err)?;
                    decoder.set_encryption_key(crypto.value, 0);
                    decoder
                        .decode_to_wav(&mut fs::File::create(&target).map_err(err)?)
                        .map_err(err)?;
                    drop(cpu);
                    drop(hca_stage);
                    self.media_check(&target)?;
                    audio = Some(self.record_audio(report_root, &target, "hca", report)?);
                }
                _ => return Err(err("unsupported USM stream codec")),
            }
        }
        let video = video.ok_or_else(|| err("USM contains no video"))?;
        let movie = output.join("movie.mkv");
        let mut args = vec![
            "-fflags".into(),
            "+genpts".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
        ];
        if let Some((num, den)) = metadata.video_frame_rate() {
            args.extend(["-r".into(), format!("{num}/{den}").into()]);
        }
        args.extend(["-i".into(), video.as_os_str().to_owned()]);
        if let Some(audio) = audio {
            args.extend([
                "-i".into(),
                audio.into_os_string(),
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "1:a:0".into(),
            ]);
        } else {
            args.extend(["-map".into(), "0:v:0".into()]);
        }
        args.extend(["-c".into(), "copy".into(), movie.as_os_str().to_owned()]);
        self.ffmpeg_to(&args, &[&movie])?;
        self.check_video_frames(&movie, expected_frames)?;
        self.video_formats(&movie, expected_frames, report_root, report)?;
        let target = output.join("usm.json");
        fs::write(&target, metadata_json).map_err(err)?;
        self.record(report_root, &target, "usm_metadata_json", report)?;
        let target = output.join("container-mask.json");
        write_json(&target, &crypto.masked)?;
        self.record(
            report_root,
            &target,
            if crypto.masked {
                "usm_masked_json"
            } else {
                "usm_plaintext_json"
            },
            report,
        )?;
        Ok(())
    }
    fn adx(&self, bytes: &[u8], target: &Path) -> Result<(), Error> {
        let (channels, rate, samples, end) = adx_layout(bytes)?;
        let wav_bytes = samples as u64 * channels as u64 * 2;
        if wav_bytes > self.max_resource_output_bytes {
            return Err(Error::Size);
        }
        // FFmpeg's ADX demuxer marks a short final packet corrupt even when it
        // contains complete ADPCM blocks. Validate those blocks first and
        // verify exact decoded PCM length instead of discarding that packet.
        let mut source = tempfile::Builder::new()
            .suffix(".adx")
            .tempfile_in(target.parent().ok_or(Error::Io)?)
            .map_err(err)?;
        source.write_all(&bytes[..end]).map_err(err)?;
        self.ffmpeg_to(
            &[
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-err_detect".into(),
                "explode".into(),
                "-i".into(),
                source.path().as_os_str().to_owned(),
                "-map".into(),
                "0:a:0".into(),
                "-af".into(),
                format!("atrim=end_sample={samples}").into(),
                "-c:a".into(),
                "pcm_s16le".into(),
                target.as_os_str().to_owned(),
            ],
            &[target],
        )?;
        let wav = fs::read(target).map_err(err)?;
        validate_wav(&wav)?;
        let mut pos = 12;
        let mut actual = 0;
        let mut format = None;
        while pos + 8 <= wav.len() {
            let len = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if &wav[pos..pos + 4] == b"data" {
                actual = len as u64;
            }
            if &wav[pos..pos + 4] == b"fmt " {
                format = Some((
                    u16::from_le_bytes(wav[pos + 10..pos + 12].try_into().unwrap()),
                    u32::from_le_bytes(wav[pos + 12..pos + 16].try_into().unwrap()),
                ));
            }
            pos += 8 + len + len % 2;
        }
        if actual != wav_bytes || format != Some((channels as u16, rate)) {
            return Err(err("ADX decoded PCM length or format mismatch"));
        }
        Ok(())
    }
    fn check_video_frames(&self, movie: &Path, expected_frames: Option<u64>) -> Result<(), Error> {
        let progress = tempfile::NamedTempFile::new_in(movie.parent().ok_or(Error::Verification)?)
            .map_err(err)?;
        self.ffmpeg(&[
            "-xerror".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-threads".into(),
            "2".into(),
            "-i".into(),
            movie.as_os_str().to_owned(),
            "-map".into(),
            "0".into(),
            "-progress".into(),
            progress.path().as_os_str().to_owned(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ])?;
        let progress = fs::read_to_string(progress.path()).map_err(err)?;
        let decoded_frames = progress
            .lines()
            .filter_map(|s| s.strip_prefix("frame="))
            .filter_map(|s| s.trim().parse::<u64>().ok())
            .next_back();
        if expected_frames.is_none() || decoded_frames != expected_frames {
            return Err(err(format!("video frame count mismatch: declared {expected_frames:?}, decoded {decoded_frames:?}")));
        }
        Ok(())
    }
    fn video_formats(
        &self,
        movie: &Path,
        expected_frames: Option<u64>,
        root: &Path,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        use crate::export_options::VideoExport;
        if matches!(self.video, VideoExport::Mp4 | VideoExport::MkvAndMp4) {
            let mp4 = movie.with_extension("mp4");
            self.encode_media(
                crate::media_backend::Encoding::Mp4,
                movie,
                &mp4,
                &[
                    "-protocol_whitelist".into(),
                    "file,pipe".into(),
                    "-i".into(),
                    movie.as_os_str().to_owned(),
                    "-map".into(),
                    "0:v:0".into(),
                    "-map".into(),
                    "0:a:0?".into(),
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "medium".into(),
                    "-crf".into(),
                    "18".into(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                    "-threads".into(),
                    "2".into(),
                    "-fps_mode".into(),
                    "passthrough".into(),
                    "-c:a".into(),
                    "aac".into(),
                    "-b:a".into(),
                    "192k".into(),
                    "-movflags".into(),
                    "+faststart".into(),
                    mp4.as_os_str().to_owned(),
                ],
            )?;
            self.check_video_frames(&mp4, expected_frames)?;
            self.record(root, &mp4, "usm_mp4", report)?;
        }
        if matches!(self.video, VideoExport::Mkv | VideoExport::MkvAndMp4) {
            self.record(root, movie, "usm_mkv", report)?;
        } else {
            fs::remove_file(movie).map_err(err)?;
        }
        Ok(())
    }
    fn media_check(&self, path: &Path) -> Result<(), Error> {
        if path.extension().is_some_and(|e| e == "wav") {
            return validate_wav(&fs::read(path).map_err(err)?);
        }

        self.ffmpeg(&[
            "-xerror".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-threads".into(),
            "2".into(),
            "-i".into(),
            path.as_os_str().to_owned(),
            "-map".into(),
            "0".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ])
    }
    fn encode_media(
        &self,
        encoding: crate::media_backend::Encoding,
        input: &Path,
        output: &Path,
        cli_args: &[std::ffi::OsString],
    ) -> Result<(), Error> {
        use crate::media_backend::Backend;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(self.media_timeout_seconds);
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let stage = match encoding {
            crate::media_backend::Encoding::Flac | crate::media_backend::Encoding::Mp3 => {
                crate::stage_limits::Stage::AudioEncode
            }
            crate::media_backend::Encoding::Mp4 => crate::stage_limits::Stage::VideoEncode,
        };
        // Keep this slot across an Auto fallback; all admission and attempts share one deadline.
        let _encoding = self.stage_gates.acquire_until(
            &self.effective_stage_limits()?,
            stage,
            &self.cancel,
            deadline,
        )?;
        if self.media_backend == Backend::Cli {
            return self.ffmpeg_deadline(cli_args, deadline, &[output]);
        }
        self.media_backend.validate()?;
        match fs::symlink_metadata(output) {
            Ok(_) => return Err(Error::Config),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(err(error)),
        }
        #[cfg(feature = "media-ffi")]
        {
            let result = {
                let _permit = self.acquire_media(deadline)?;
                let _cpu = self.acquire_cpu(deadline)?;
                crate::media_ffi::controlled(self.cancel.clone(), deadline, || match encoding {
                    crate::media_backend::Encoding::Flac => {
                        crate::media_ffi::convert_wav_to_flac(input, output)
                    }
                    crate::media_backend::Encoding::Mp3 => {
                        crate::media_ffi::convert_wav_to_mp3(input, output)
                    }
                    crate::media_backend::Encoding::Mp4 => {
                        crate::media_ffi::convert_video_to_mp4(input, output)
                    }
                })
            };
            match result {
                Ok(()) => {
                    self.ffi_conversions
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                Err(error) => {
                    match fs::remove_file(output) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(err(e)),
                    }
                    match error {
                        crate::media_ffi::MediaError::Cancelled => return Err(Error::Cancelled),
                        crate::media_ffi::MediaError::Timeout => {
                            return Err(err("media conversion timed out"))
                        }
                        _ if self.media_backend == Backend::Ffi => {
                            return Err(err("FFI media conversion failed"))
                        }
                        _ => {}
                    }
                }
            }
        }
        #[cfg(not(feature = "media-ffi"))]
        let _ = (encoding, input);
        self.media_fallbacks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            stage = "media_cli_fallback",
            "Media encoding fell back to CLI"
        );
        self.ffmpeg_deadline(cli_args, deadline, &[output])
    }
    fn cpu_deadline(&self) -> std::time::Instant {
        std::time::Instant::now()
            + std::time::Duration::from_secs(self.stage_limits.wait_timeout_seconds)
    }
    fn acquire_cpu(&self, deadline: std::time::Instant) -> Result<CpuPermits<'_>, Error> {
        let result = (|| {
            let local = if self.cpu.limit_stages {
                Some(self.cpu_gate.acquire(
                    self.cpu.budget_for_cpus(self.detected_cpus)?,
                    &self.cancel,
                    deadline,
                )?)
            } else {
                None
            };
            let shared = self
                .service_cpu_gate
                .as_ref()
                .map(|(gate, limit)| gate.acquire(*limit, &self.cancel, deadline))
                .transpose()?;
            crate::cpu_throttle::wait(
                &self.cpu.throttle,
                self.cpu.budget_for_cpus(self.detected_cpus)?,
                &self.cancel,
                deadline,
            )?;
            Ok((local, shared))
        })();
        result.map_err(|error| match error {
            Error::Export(message) if message == "media process timed out" => {
                Error::Export("CPU stage admission timed out".into())
            }
            other => other,
        })
    }
    pub(crate) fn set_service_cpu_gate(
        &mut self,
        gate: std::sync::Arc<crate::media_gate::Gate>,
        limit: Option<usize>,
    ) {
        self.service_cpu_gate = limit.map(|limit| (gate, limit));
    }
    pub(crate) fn set_service_resource_budget(
        &mut self,
        budget: Option<std::sync::Arc<crate::resource_budget::Budget>>,
    ) {
        self.service_resource_budget = budget;
    }
    pub(crate) fn set_service_media_gate(
        &mut self,
        gate: std::sync::Arc<crate::media_gate::Gate>,
        limit: usize,
    ) {
        self.service_media_gate = Some((gate, limit));
    }
    fn acquire_media(
        &self,
        deadline: std::time::Instant,
    ) -> Result<
        (
            crate::media_gate::Permit<'_>,
            Option<crate::media_gate::Permit<'_>>,
        ),
        Error,
    > {
        let local = self
            .media_gate
            .acquire(self.media_concurrency, &self.cancel, deadline)?;
        let shared = self
            .service_media_gate
            .as_ref()
            .map(|(gate, limit)| gate.acquire(*limit, &self.cancel, deadline))
            .transpose()?;
        Ok((local, shared))
    }
    fn ffmpeg(&self, args: &[std::ffi::OsString]) -> Result<(), Error> {
        self.ffmpeg_to(args, &[])
    }
    /// `outputs` are files the command creates without `-y`; they are removed before a retry so
    /// every attempt starts from a fresh output rather than a partial one.
    fn ffmpeg_to(&self, args: &[std::ffi::OsString], outputs: &[&Path]) -> Result<(), Error> {
        self.ffmpeg_deadline(
            args,
            std::time::Instant::now() + std::time::Duration::from_secs(self.media_timeout_seconds),
            outputs,
        )
    }
    /// Runs one FFmpeg command under `media_retry`. All attempts, admission waits and backoff
    /// share `deadline`; a retry that could not start before it is not attempted.
    fn ffmpeg_deadline(
        &self,
        args: &[std::ffi::OsString],
        deadline: std::time::Instant,
        outputs: &[&Path],
    ) -> Result<(), Error> {
        use std::sync::atomic::Ordering;
        let mut retry = 0;
        loop {
            let error = match self.ffmpeg_attempt(args, deadline) {
                Ok(()) => return Ok(()),
                Err(MediaFailure::Final(error)) => return Err(error),
                Err(MediaFailure::Transient(error)) => error,
            };
            if retry + 1 >= self.media_retry.attempts || self.cancel.load(Ordering::Relaxed) {
                return Err(error);
            }
            let delay = self.media_retry.delay(retry);
            let resume = match std::time::Instant::now().checked_add(delay) {
                Some(resume) if resume < deadline => resume,
                _ => return Err(error),
            };
            for output in outputs {
                match fs::remove_file(output) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(err(e)),
                }
            }
            retry += 1;
            self.media_retries.fetch_add(1, Ordering::Relaxed);
            // Paths and stderr are deliberately not logged here.
            tracing::warn!(
                stage = "media_retry",
                attempt = retry,
                max_attempts = self.media_retry.attempts,
                delay_ms = delay.as_millis() as u64,
                "Media process failed transiently; retrying"
            );
            // Backoff holds no media or CPU permit and observes cancellation every 20 ms.
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let now = std::time::Instant::now();
                if now >= resume {
                    break;
                }
                std::thread::sleep((resume - now).min(std::time::Duration::from_millis(20)));
            }
        }
    }
    fn ffmpeg_attempt(
        &self,
        args: &[std::ffi::OsString],
        deadline: std::time::Instant,
    ) -> Result<(), MediaFailure> {
        let _permit = self.acquire_media(deadline).map_err(MediaFailure::Final)?;
        let _cpu = self.acquire_cpu(deadline).map_err(MediaFailure::Final)?;
        let fatal = |e: std::io::Error| MediaFailure::Final(err(e));
        let stderr = tempfile::tempfile().map_err(fatal)?;
        let mut child = Command::new(&self.ffmpeg)
            .env_remove(&self.cri_key_env)
            .args(["-v", "error", "-nostdin"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(stderr.try_clone().map_err(fatal)?)
            .spawn()
            .map_err(|e| {
                if transient_spawn_error(&e) {
                    MediaFailure::Transient(err(e))
                } else {
                    MediaFailure::Final(err(e))
                }
            })?;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(fatal)? {
                break status;
            }
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed)
                || std::time::Instant::now() >= deadline
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MediaFailure::Final(err(
                    "media decoder cancelled or timed out",
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        use std::io::{Read, Seek, SeekFrom};
        let mut stderr = stderr;
        stderr.seek(SeekFrom::Start(0)).map_err(fatal)?;
        let mut diagnostic = String::new();
        stderr
            .take(8192)
            .read_to_string(&mut diagnostic)
            .map_err(fatal)?;
        if !status.success() || !diagnostic.is_empty() {
            let transient = transient_media_failure(&status.to_string(), &diagnostic);
            let error = err(format!("media decode: {status}: {diagnostic}"));
            return Err(if transient {
                MediaFailure::Transient(error)
            } else {
                MediaFailure::Final(error)
            });
        }
        Ok(())
    }
    fn record_audio(
        &self,
        root: &Path,
        wav: &Path,
        codec: &str,
        report: &mut ResourceReport,
    ) -> Result<PathBuf, Error> {
        use crate::export_options::AudioExport;
        let flac = self.audio.contains(AudioExport::Flac);
        // At least one lossless preservation representation always remains.
        let keep_wav = self.audio.contains(AudioExport::Wav) || !flac;
        let mp3 = if self.audio.contains(AudioExport::Mp3) {
            Some(self.encode_mp3(root, wav)?)
        } else {
            None
        };
        if keep_wav {
            self.record(root, wav, &format!("{codec}_wav"), report)?;
        }
        if let Some(mp3) = mp3 {
            self.record(root, &mp3, &format!("{codec}_mp3"), report)?;
        }
        if !flac {
            return Ok(wav.into());
        }
        let target = wav.with_extension("flac");
        self.encode_media(
            crate::media_backend::Encoding::Flac,
            wav,
            &target,
            &[
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-i".into(),
                wav.as_os_str().to_owned(),
                "-map".into(),
                "0:a:0".into(),
                "-c:a".into(),
                "flac".into(),
                target.as_os_str().to_owned(),
            ],
        )?;
        let decoded = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile_in(root)
            .map_err(err)?;
        self.ffmpeg(&[
            "-y".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-i".into(),
            target.as_os_str().to_owned(),
            "-map".into(),
            "0:a:0".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            decoded.path().as_os_str().to_owned(),
        ])?;
        let original = fs::read(wav).map_err(err)?;
        let roundtrip = fs::read(decoded.path()).map_err(err)?;
        if pcm_identity(&original)? != pcm_identity(&roundtrip)? {
            return Err(err("FLAC PCM roundtrip mismatch"));
        }
        self.record(root, &target, &format!("{codec}_flac"), report)?;
        if !keep_wav {
            fs::remove_file(wav).map_err(err)?;
        }
        Ok(target)
    }
    fn encode_mp3(&self, root: &Path, wav: &Path) -> Result<PathBuf, Error> {
        let original = fs::read(wav).map_err(err)?;
        let (channels, rate, pcm) = pcm_identity(&original)?;
        if !matches!(channels, 1 | 2) || pcm.is_empty() {
            return Err(err("MP3 requires nonempty mono/stereo PCM"));
        }
        let bitrate = match rate {
            32000 | 44100 | 48000 => "192k",
            16000 | 22050 | 24000 => "128k",
            8000 | 11025 | 12000 => "64k",
            _ => return Err(err("MP3 sample rate is unsupported without resampling")),
        };
        let target = wav.with_extension("mp3");
        self.encode_media(
            crate::media_backend::Encoding::Mp3,
            wav,
            &target,
            &[
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-i".into(),
                wav.as_os_str().to_owned(),
                "-map".into(),
                "0:a:0".into(),
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                bitrate.into(),
                target.as_os_str().to_owned(),
            ],
        )?;
        let decoded = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile_in(root)
            .map_err(err)?;
        self.ffmpeg(&[
            "-y".into(),
            "-xerror".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-i".into(),
            target.as_os_str().to_owned(),
            "-map".into(),
            "0:a:0".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            decoded.path().as_os_str().to_owned(),
        ])?;
        let roundtrip = fs::read(decoded.path()).map_err(err)?;
        let (decoded_channels, decoded_rate, decoded_pcm) = pcm_identity(&roundtrip)?;
        let original_frames = pcm.len() / (usize::from(channels) * 2);
        let decoded_frames = decoded_pcm.len() / (usize::from(decoded_channels) * 2);
        if channels != decoded_channels
            || rate != decoded_rate
            || decoded_pcm.is_empty()
            || original_frames.abs_diff(decoded_frames) > 1152
        {
            return Err(err("MP3 channel/rate/duration verification failed"));
        }
        Ok(target)
    }
    fn record(
        &self,
        root: &Path,
        path: &Path,
        kind: &str,
        report: &mut ResourceReport,
    ) -> Result<(), Error> {
        let bytes = fs::read(path).map_err(err)?;
        let prior: u64 = report.outputs.iter().map(|o| o.bytes).sum();
        if prior.saturating_add(bytes.len() as u64) > self.max_resource_output_bytes {
            return Err(Error::Size);
        }
        if kind.contains("json") {
            let _: sonic_rs::Value = sonic_rs::from_slice(&bytes).map_err(err)?;
        }
        if kind == "image_png" {
            let decoder = png::Decoder::new(Cursor::new(&bytes));
            let mut reader = decoder.read_info().map_err(err)?;
            let mut buffer = vec![0; reader.output_buffer_size().ok_or(Error::Size)?];
            reader.next_frame(&mut buffer).map_err(err)?;
        }
        if kind.starts_with("image_") && kind != "image_png" {
            self.media_check(path)?;
        }
        report.outputs.push(OutputRecord {
            path: path
                .strip_prefix(root)
                .map_err(err)?
                .to_string_lossy()
                .replace('\\', "/"),
            kind: kind.into(),
            bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            object: None,
        });
        Ok(())
    }
}

fn pcm_identity(bytes: &[u8]) -> Result<(u16, u32, &[u8]), Error> {
    validate_wav(bytes)?;
    let mut pos = 12;
    let mut channels = 0;
    let mut rate = 0;
    let mut pcm = None;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &bytes[pos..pos + 4] == b"fmt " {
            channels = u16::from_le_bytes(bytes[pos + 10..pos + 12].try_into().unwrap());
            rate = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
        }
        if &bytes[pos..pos + 4] == b"data" {
            pcm = Some(&bytes[pos + 8..pos + 8 + len]);
        }
        pos += 8 + len + len % 2;
    }
    Ok((channels, rate, pcm.ok_or(Error::Verification)?))
}

// PCM WAV validation is independent of the HCA decoder; require complete frames,
// correct RIFF length, and coherent sample rate/channel/block alignment fields.
fn validate_wav(bytes: &[u8]) -> Result<(), Error> {
    let invalid = || err("invalid PCM WAV output");
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid());
    }
    let u32le = |p: usize| u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap());
    if u32le(4) as usize + 8 != bytes.len() {
        return Err(invalid());
    }
    let mut pos = 12;
    let mut alignment = None;
    let mut data = None;
    while pos + 8 <= bytes.len() {
        let size = u32le(pos + 4) as usize;
        let begin = pos + 8;
        let end = begin
            .checked_add(size)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(invalid)?;
        match &bytes[pos..pos + 4] {
            b"fmt " => {
                if size < 16 {
                    return Err(invalid());
                }
                let format = u16::from_le_bytes(bytes[begin..begin + 2].try_into().unwrap());
                let channels = u16::from_le_bytes(bytes[begin + 2..begin + 4].try_into().unwrap());
                let rate = u32le(begin + 4);
                let byte_rate = u32le(begin + 8);
                let block = u16::from_le_bytes(bytes[begin + 12..begin + 14].try_into().unwrap());
                let bits = u16::from_le_bytes(bytes[begin + 14..begin + 16].try_into().unwrap());
                if format != 1
                    || channels == 0
                    || channels > 16
                    || rate == 0
                    || bits != 16
                    || block != channels * 2
                    || byte_rate as u64 != rate as u64 * block as u64
                {
                    return Err(invalid());
                }
                alignment = Some(block as usize);
            }
            b"data" if data.replace(size).is_some() => return Err(invalid()),
            _ => {}
        }
        pos = end + (size % 2);
    }
    if pos != bytes.len() || !matches!((data,alignment),(Some(n),Some(a)) if n>0 && n%a==0) {
        return Err(invalid());
    }
    Ok(())
}

fn is_moc_object(
    studio: &unity_rs_core::studio::Studio,
    object: unity_rs_core::studio::StudioObject<'_>,
) -> Result<bool, Error> {
    let file = &studio.collection().serialized_files()[object.file_index()].file;
    let tree = file.object_type_tree(object.object_index()).map_err(err)?;
    let fields: Vec<_> = tree.nodes.iter().filter(|n| n.level == 1).collect();
    if fields.last().is_none_or(|n| n.field_name != "_bytes") {
        return Ok(false);
    }
    let (_, data) = unity_rs_core::monobehaviour::read_mono_behaviour_with_script_data(
        file,
        object.object_index(),
        unity_rs_core::monobehaviour::MonoBehaviourReadLimits::default(),
    )
    .map_err(err)?;
    if data.len() < 8 {
        return Ok(false);
    }
    let prefix = data
        .subregion(4, 4)
        .map_err(err)?
        .read_to_vec(4)
        .map_err(err)?;
    Ok(prefix == b"MOC3")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    fn config(root: &Path) -> ExportConfig {
        ExportConfig {
            logging: None,
            input: root.into(),
            paths: Vec::new(),
            selection: Default::default(),
            read_kinds: Default::default(),
            cri: Default::default(),
            raw_bundles: None,
            image: Default::default(),
            audio: Default::default(),
            video: Default::default(),
            media_backend: Default::default(),
            ffi_conversions: Default::default(),
            media_fallbacks: Default::default(),
            output: root.join("out"),
            retain_outputs: false,
            cache_directory: None,
            cache_revision: String::new(),
            cache_max_bytes: None,
            cache_max_entries: None,
            cri_key_env: "UNUSED_TEST_KEY".into(),
            split_acb_xor_env: None,
            concurrency: 1,
            cpu: Default::default(),
            detected_cpus: crate::cpu_policy::available_cpus(),
            cpu_gate: Default::default(),
            service_cpu_gate: None,
            stage_limits: Default::default(),
            stage_gates: Default::default(),
            max_in_flight_bundle_bytes: 0,
            local_resource_budget: Default::default(),
            service_resource_budget: None,
            media_concurrency: 2,
            media_gate: Default::default(),
            service_media_gate: None,
            media_timeout_seconds: 120,
            media_retry: Default::default(),
            media_retries: Default::default(),
            cancel: Default::default(),
            ffmpeg: "unused".into(),
            max_resource_output_bytes: 16 * 1024 * 1024,
        }
    }
    #[test]
    fn media_backend_config_is_explicit_and_cancel_does_not_fallback() {
        use crate::media_backend::{Backend, Encoding};
        assert!(yaml_serde::from_str::<Backend>("unknown").is_err());
        assert_eq!(Backend::default(), Backend::Cli);
        assert!(Backend::Auto.validate().is_ok());
        assert_eq!(Backend::Ffi.validate().is_ok(), cfg!(feature = "media-ffi"));
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.media_backend = Backend::Auto;
        cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            cfg.encode_media(
                Encoding::Flac,
                &directory.path().join("in.wav"),
                &directory.path().join("out.flac"),
                &[]
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(
            cfg.media_fallbacks
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for video format and independent decode checks"]
    fn video_formats_preserve_source_and_validate_mp4_frames_and_audio() {
        check_video_backend(crate::media_backend::Backend::Cli);
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for actual backend encoding and independent verification"]
    fn auto_video_formats_preserve_source_and_validate_mp4_frames_and_audio() {
        check_video_backend(crate::media_backend::Backend::Auto);
    }
    #[cfg(feature = "media-ffi")]
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for actual FFI export integration"]
    fn ffi_video_formats_preserve_source_and_validate_mp4_frames_and_audio() {
        check_video_backend(crate::media_backend::Backend::Ffi);
    }
    #[cfg(feature = "media-ffi")]
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG and ffprobe for real timing fallback verification"]
    fn auto_video_timing_fallback_cleans_partial_output_and_matches_cli() {
        use crate::media_backend::{Backend, Encoding};
        use std::sync::atomic::Ordering;
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        let ffprobe = cfg.ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        for (name, filter) in [
            ("mpeg-pcm", "null"),
            ("offset", "setpts=PTS+2/TB"),
            ("variable", "setpts=if(lt(N\\,4)\\,N\\,2*N-4)/(25*TB)"),
        ] {
            let source = directory.path().join(format!("{name}.mkv"));
            cfg.ffmpeg(&[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "testsrc2=size=16x16:rate=25:duration=0.48".into(),
                "-vf".into(),
                filter.into(),
                "-fps_mode".into(),
                "passthrough".into(),
                "-c:v".into(),
                "ffv1".into(),
                source.as_os_str().to_owned(),
            ])
            .unwrap();
            if name == "mpeg-pcm" {
                fs::remove_file(&source).unwrap();
                let elementary = directory.path().join("source.m2v");
                cfg.ffmpeg(&[
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    "testsrc2=size=16x16:rate=25:duration=0.48".into(),
                    "-c:v".into(),
                    "mpeg2video".into(),
                    elementary.as_os_str().to_owned(),
                ])
                .unwrap();
                cfg.ffmpeg(&[
                    "-fflags".into(),
                    "+genpts".into(),
                    "-r".into(),
                    "25".into(),
                    "-i".into(),
                    elementary.as_os_str().to_owned(),
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    "sine=sample_rate=44100:duration=0.48".into(),
                    "-c:v".into(),
                    "copy".into(),
                    "-c:a".into(),
                    "pcm_s16le".into(),
                    "-t".into(),
                    "0.48".into(),
                    source.as_os_str().to_owned(),
                ])
                .unwrap();
            }
            let original = fs::read(&source).unwrap();
            let output = directory.path().join(format!("{name}.mp4"));
            let baseline = directory.path().join(format!("{name}-cli.mp4"));
            let args = |target: &Path| {
                vec![
                    "-i".into(),
                    source.as_os_str().to_owned(),
                    "-c:v".into(),
                    "libx264".into(),
                    "-preset".into(),
                    "medium".into(),
                    "-crf".into(),
                    "18".into(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                    "-movflags".into(),
                    "+faststart".into(),
                    target.as_os_str().to_owned(),
                ]
            };
            cfg.media_backend = Backend::Ffi;
            assert!(cfg
                .encode_media(Encoding::Mp4, &source, &output, &args(&output))
                .is_err());
            assert!(!output.exists(), "failed FFI must remove partial output");
            assert_eq!(cfg.media_fallbacks.load(Ordering::Relaxed), 0);
            cfg.media_backend = Backend::Auto;
            cfg.encode_media(Encoding::Mp4, &source, &output, &args(&output))
                .unwrap();
            assert_eq!(cfg.media_fallbacks.swap(0, Ordering::Relaxed), 1);
            assert_eq!(cfg.ffi_conversions.load(Ordering::Relaxed), 0);
            cfg.ffmpeg(&args(&baseline)).unwrap();
            let frames = |path: &Path| {
                let probe = Command::new(&ffprobe)
                    .args([
                        "-v",
                        "error",
                        "-show_entries",
                        "frame=media_type,best_effort_timestamp_time,pkt_duration_time",
                        "-of",
                        "csv",
                    ])
                    .arg(path)
                    .output()
                    .unwrap();
                assert!(probe.status.success());
                assert!(!probe.stdout.is_empty());
                probe.stdout
            };
            assert_eq!(frames(&output), frames(&baseline));
            assert_eq!(fs::read(&source).unwrap(), original);
        }
    }
    fn check_video_backend(backend: crate::media_backend::Backend) {
        use crate::export_options::VideoExport;
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.stage_limits.video_encode = Some(1);
        cfg.media_backend = backend;
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        let source = directory.path().join("source.mkv");
        cfg.ffmpeg(&[
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc2=size=16x16:rate=25:duration=0.32".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:sample_rate=48000:duration=0.32".into(),
            "-c:v".into(),
            "ffv1".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            source.as_os_str().to_owned(),
        ])
        .unwrap();
        let original = fs::read(&source).unwrap();
        for (index, mode) in [
            VideoExport::Source,
            VideoExport::Mkv,
            VideoExport::Mp4,
            VideoExport::MkvAndMp4,
        ]
        .into_iter()
        .enumerate()
        {
            cfg.video = mode;
            let root = directory.path().join(index.to_string());
            fs::create_dir(&root).unwrap();
            let movie = root.join("movie.mkv");
            fs::copy(&source, &movie).unwrap();
            let mut report = ResourceReport::default();
            cfg.check_video_frames(&movie, Some(8)).unwrap();
            cfg.video_formats(&movie, Some(8), &root, &mut report)
                .unwrap();
            let mkv = matches!(mode, VideoExport::Mkv | VideoExport::MkvAndMp4);
            let mp4 = matches!(mode, VideoExport::Mp4 | VideoExport::MkvAndMp4);
            assert_eq!(movie.exists(), mkv);
            assert_eq!(root.join("movie.mp4").exists(), mp4);
            assert_eq!(report.outputs.len(), usize::from(mkv) + usize::from(mp4));
            if mkv {
                assert_eq!(fs::read(&movie).unwrap(), original);
            }
            if mp4 {
                let video = root.join("frames.yuv");
                let audio = root.join("audio.pcm");
                cfg.ffmpeg(&[
                    "-i".into(),
                    root.join("movie.mp4").into_os_string(),
                    "-map".into(),
                    "0:v:0".into(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                    "-f".into(),
                    "rawvideo".into(),
                    video.as_os_str().to_owned(),
                ])
                .unwrap();
                assert_eq!(fs::metadata(video).unwrap().len(), 8 * 16 * 16 * 3 / 2);
                cfg.ffmpeg(&[
                    "-i".into(),
                    root.join("movie.mp4").into_os_string(),
                    "-map".into(),
                    "0:a:0".into(),
                    "-f".into(),
                    "s16le".into(),
                    audio.as_os_str().to_owned(),
                ])
                .unwrap();
                assert!(fs::metadata(audio).unwrap().len() >= 15_000 * 2);
                assert!(cfg
                    .check_video_frames(&root.join("movie.mp4"), Some(9))
                    .is_err());
            }
            assert_eq!(fs::read(&source).unwrap(), original);
        }
        assert!(yaml_serde::from_str::<VideoExport>("avi").is_err());
        let ffi = cfg
            .ffi_conversions
            .load(std::sync::atomic::Ordering::Relaxed);
        let fallback = cfg
            .media_fallbacks
            .load(std::sync::atomic::Ordering::Relaxed);
        if backend == crate::media_backend::Backend::Cli {
            assert_eq!((ffi, fallback), (0, 0));
        } else if cfg!(feature = "media-ffi") {
            assert!(ffi > 0);
            assert_eq!(fallback, 0);
        } else {
            assert_eq!(ffi, 0);
            assert!(fallback > 0);
        }
    }
    #[test]
    fn audio_format_lists_are_canonical_and_reject_invalid_selections() {
        use crate::export_options::AudioFormats;
        for format in ["wav", "flac", "mp3"] {
            let single: AudioFormats = yaml_serde::from_str(format).unwrap();
            let list: AudioFormats = yaml_serde::from_str(&format!("[{format}]")).unwrap();
            assert_eq!(single, list);
            assert_eq!(
                sonic_rs::to_string(&single).unwrap(),
                sonic_rs::to_string(&list).unwrap()
            );
        }
        let a: AudioFormats = yaml_serde::from_str("[mp3, wav, flac]").unwrap();
        let b: AudioFormats = yaml_serde::from_str("[flac, mp3, wav]").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            sonic_rs::to_string(&a).unwrap(),
            "[\"wav\",\"flac\",\"mp3\"]"
        );
        for bad in [
            "[]",
            "[wav, wav]",
            "[ogg]",
            "[wav, flac, mp3, wav]",
            "{formats: [wav]}",
        ] {
            assert!(yaml_serde::from_str::<AudioFormats>(bad).is_err());
        }
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for simultaneous audio-format verification"]
    fn simultaneous_audio_formats_preserve_requested_lossless_outputs() {
        check_audio_backend(crate::media_backend::Backend::Cli);
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for actual backend encoding and independent verification"]
    fn auto_simultaneous_audio_formats_preserve_requested_lossless_outputs() {
        check_audio_backend(crate::media_backend::Backend::Auto);
    }
    #[cfg(feature = "media-ffi")]
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for actual FFI export integration"]
    fn ffi_simultaneous_audio_formats_preserve_requested_lossless_outputs() {
        check_audio_backend(crate::media_backend::Backend::Ffi);
    }
    fn check_audio_backend(backend: crate::media_backend::Backend) {
        use crate::export_options::AudioExport;
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.media_backend = backend;
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        let source = directory.path().join("source.wav");
        cfg.ffmpeg(&[
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:sample_rate=48000:duration=0.25".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            source.as_os_str().to_owned(),
        ])
        .unwrap();
        let original = fs::read(&source).unwrap();
        for (i, formats) in [
            "[wav]",
            "[flac]",
            "[mp3]",
            "[wav, flac]",
            "[wav, mp3]",
            "[flac, mp3]",
            "[wav, flac, mp3]",
        ]
        .into_iter()
        .enumerate()
        {
            cfg.audio = yaml_serde::from_str(formats).unwrap();
            let root = directory.path().join(i.to_string());
            fs::create_dir(&root).unwrap();
            let wav = root.join("audio.wav");
            fs::copy(&source, &wav).unwrap();
            let mut report = ResourceReport::default();
            let mux = cfg
                .record_audio(&root, &wav, "fixture", &mut report)
                .unwrap();
            let flac = cfg.audio.contains(AudioExport::Flac);
            let mp3 = cfg.audio.contains(AudioExport::Mp3);
            let keep_wav = cfg.audio.contains(AudioExport::Wav) || !flac;
            assert_eq!(wav.exists(), keep_wav);
            assert_eq!(wav.with_extension("flac").exists(), flac);
            assert_eq!(wav.with_extension("mp3").exists(), mp3);
            assert_eq!(
                report.outputs.len(),
                usize::from(keep_wav) + usize::from(flac) + usize::from(mp3)
            );
            assert_eq!(
                report
                    .outputs
                    .iter()
                    .map(|o| &o.path)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                report.outputs.len()
            );
            assert_eq!(mux.extension().unwrap(), if flac { "flac" } else { "wav" });
            if keep_wav {
                assert_eq!(fs::read(&wav).unwrap(), original);
            }
            if flac {
                let decoded = root.join("decoded.wav");
                cfg.ffmpeg(&[
                    "-i".into(),
                    wav.with_extension("flac").into_os_string(),
                    "-c:a".into(),
                    "pcm_s16le".into(),
                    decoded.as_os_str().to_owned(),
                ])
                .unwrap();
                assert_eq!(
                    pcm_identity(&fs::read(decoded).unwrap()).unwrap(),
                    pcm_identity(&original).unwrap()
                );
            }
        }
        let ffi = cfg
            .ffi_conversions
            .load(std::sync::atomic::Ordering::Relaxed);
        let fallback = cfg
            .media_fallbacks
            .load(std::sync::atomic::Ordering::Relaxed);
        if backend == crate::media_backend::Backend::Cli {
            assert_eq!((ffi, fallback), (0, 0));
        } else if cfg!(feature = "media-ffi") {
            assert!(ffi > 0);
            assert_eq!(fallback, 0);
        } else {
            assert_eq!(ffi, 0);
            assert!(fallback > 0);
        }
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for MP3 encode/decode and PCM preservation checks"]
    fn mp3_output_preserves_pcm_and_verifies_supported_audio_shapes() {
        use crate::export_options::AudioExport;
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        cfg.audio = AudioExport::Mp3.into();
        for rate in [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000] {
            for channels in [1, 2] {
                let wav = directory.path().join(format!("{rate}-{channels}.wav"));
                cfg.ffmpeg(&[
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    format!("sine=frequency=440:sample_rate={rate}:duration=0.25").into(),
                    "-ac".into(),
                    channels.to_string().into(),
                    "-c:a".into(),
                    "pcm_s16le".into(),
                    wav.as_os_str().to_owned(),
                ])
                .unwrap();
                let original = fs::read(&wav).unwrap();
                let mut report = ResourceReport::default();
                assert_eq!(
                    cfg.record_audio(directory.path(), &wav, "fixture", &mut report)
                        .unwrap(),
                    wav
                );
                assert_eq!(fs::read(&wav).unwrap(), original);
                assert_eq!(report.outputs.len(), 2);
                assert_eq!(report.outputs[0].kind, "fixture_wav");
                assert_eq!(report.outputs[1].kind, "fixture_mp3");
                let raw = wav.with_extension("pcm");
                cfg.ffmpeg(&[
                    "-i".into(),
                    wav.with_extension("mp3").into_os_string(),
                    "-map".into(),
                    "0:a:0".into(),
                    "-f".into(),
                    "s16le".into(),
                    raw.as_os_str().to_owned(),
                ])
                .unwrap();
                let decoded = fs::read(&raw).unwrap();
                assert!(decoded.iter().any(|b| *b != 0));
                let frames = decoded.len() / (channels * 2);
                assert!(frames.abs_diff(rate / 4) <= 1152);
            }
        }
        for (rate, channels) in [(48000, 3), (12345, 1)] {
            let wav = directory
                .path()
                .join(format!("unsupported-{rate}-{channels}.wav"));
            cfg.ffmpeg(&[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("sine=sample_rate={rate}:duration=0.25").into(),
                "-ac".into(),
                channels.to_string().into(),
                "-c:a".into(),
                "pcm_s16le".into(),
                wav.as_os_str().to_owned(),
            ])
            .unwrap();
            let original = fs::read(&wav).unwrap();
            let mut report = ResourceReport::default();
            assert!(cfg
                .record_audio(directory.path(), &wav, "fixture", &mut report)
                .is_err());
            assert!(report.outputs.is_empty() && !wav.with_extension("mp3").exists());
            assert_eq!(fs::read(wav).unwrap(), original);
        }
        let acb = directory.path().join("acb");
        fs::create_dir(&acb).unwrap();
        let mut report = ResourceReport::default();
        cfg.acb(
            &synthetic_acb(0x12345678),
            &acb,
            0x12345678,
            &mut report,
            &acb,
        )
        .unwrap();
        assert!(acb.join("00000.wav").is_file() && acb.join("00000.mp3").is_file());
        assert!(report.outputs.iter().any(|o| o.kind == "hca_mp3"));
    }

    #[cfg(unix)]
    #[test]
    fn service_media_gate_bounds_independent_exports_and_releases_cancelled_waiters() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{atomic::Ordering, Arc};
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("shared-media-fixture");
        let log = directory.path().join("shared-events");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho start >> '{}'\nsleep 0.05\necho end >> '{}'\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let shared = Arc::new(crate::media_gate::Gate::default());
        let configs: Vec<_> = (0..2)
            .map(|_| {
                let mut cfg = config(directory.path());
                cfg.ffmpeg = script.clone();
                cfg.media_concurrency = 4;
                cfg.set_service_media_gate(shared.clone(), 1);
                Arc::new(cfg)
            })
            .collect();
        assert!(!Arc::ptr_eq(&configs[0].media_gate, &configs[1].media_gate));
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let cfg = configs[index % 2].clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cfg.ffmpeg(&[])
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let events = fs::read_to_string(&log).unwrap();
        assert_eq!(
            events.lines().collect::<Vec<_>>(),
            ["start", "end"].repeat(8)
        );
        fs::remove_file(&log).unwrap();
        let hold = shared
            .acquire(
                1,
                &configs[0].cancel,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        let cfg = configs[1].clone();
        let waiter = std::thread::spawn(move || cfg.ffmpeg(&[]));
        std::thread::sleep(std::time::Duration::from_millis(50));
        configs[1].cancel.store(true, Ordering::Relaxed);
        assert!(matches!(waiter.join().unwrap(), Err(Error::Cancelled)));
        assert!(!log.exists());
        // Deadline while waiting for the service gate also releases the local slot.
        configs[1].cancel.store(false, Ordering::Relaxed);
        assert!(configs[1]
            .ffmpeg_deadline(
                &[],
                std::time::Instant::now() + std::time::Duration::from_millis(30),
                &[]
            )
            .is_err());
        assert!(!log.exists());
        drop(hold);
        configs[1].ffmpeg(&[]).unwrap();
        assert_eq!(fs::read_to_string(log).unwrap(), "start\nend\n");
    }

    /// Fake FFmpeg: counts invocations, fails the first `fails` with `failure` on stderr after
    /// writing a partial output, refuses to overwrite an existing output, then succeeds.
    #[cfg(unix)]
    fn flaky_media_tool(directory: &Path, fails: usize, failure: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join(format!("flaky-media-{fails}"));
        let count = directory.join(format!("flaky-count-{fails}"));
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nn=$(cat '{count}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{count}'\nfor target do :; done\nif [ -e \"$target\" ]; then echo 'output already exists' >&2; exit 1; fi\nif [ $n -le {fails} ]; then printf partial > \"$target\"; {failure}; fi\nprintf complete > \"$target\"\n",
                count = count.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        (script, count)
    }
    #[cfg(unix)]
    fn invocations(count: &Path) -> usize {
        fs::read_to_string(count)
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }
    #[test]
    fn media_retry_config_defaults_to_single_attempt_and_is_bounded() {
        use crate::export_options::MediaRetry;
        let base = "input: in\noutput: out\ncri_key_env: KEY\nffmpeg: ffmpeg\n";
        let cfg: ExportConfig = yaml_serde::from_str(base).unwrap();
        assert_eq!(cfg.media_retry, MediaRetry::default());
        assert_eq!(cfg.media_retry.attempts, 1);
        cfg.validate().unwrap();
        for valid in [
            "{attempts: 8, delay_ms: 0, max_delay_ms: 0}",
            "{attempts: 4, delay_ms: 1000, max_delay_ms: 4000}",
            "{attempts: 2, delay_ms: 60000, max_delay_ms: 60000}",
            "{attempts: 3}",
        ] {
            let cfg: ExportConfig =
                yaml_serde::from_str(&format!("{base}media_retry: {valid}\n")).unwrap();
            assert!(cfg.validate().is_ok(), "rejected {valid}");
        }
        for invalid in [
            "{attempts: 0}",
            "{attempts: 9}",
            "{attempts: 2, delay_ms: 60001, max_delay_ms: 60001}",
            "{attempts: 2, max_delay_ms: 60001}",
            "{attempts: 2, delay_ms: 5000, max_delay_ms: 4000}",
        ] {
            let cfg: ExportConfig =
                yaml_serde::from_str(&format!("{base}media_retry: {invalid}\n")).unwrap();
            assert!(cfg.validate().is_err(), "accepted {invalid}");
        }
        for unknown in [
            "{attempts: 2, initial_backoff_ms: 10}",
            "{attempts: 2, jitter: true}",
        ] {
            assert!(
                yaml_serde::from_str::<ExportConfig>(&format!("{base}media_retry: {unknown}\n"))
                    .is_err(),
                "accepted {unknown}"
            );
        }
        let retry = MediaRetry {
            attempts: 8,
            delay_ms: 1000,
            max_delay_ms: 4000,
        };
        let delays: Vec<_> = (0..4).map(|n| retry.delay(n).as_millis()).collect();
        assert_eq!(delays, [1000, 2000, 4000, 4000]);
        assert_eq!(retry.delay(200).as_millis(), 4000);
    }
    #[cfg(unix)]
    #[test]
    fn media_retry_recovers_transient_failures_with_fresh_output() {
        use std::sync::atomic::Ordering;
        let transient = "echo 'Resource temporarily unavailable' >&2; exit 1";
        for (attempts, succeeds) in [(1, false), (2, false), (3, true), (5, true)] {
            let directory = tempfile::tempdir().unwrap();
            let (script, count) = flaky_media_tool(directory.path(), 2, transient);
            let mut cfg = config(directory.path());
            cfg.ffmpeg = script;
            cfg.media_retry.attempts = attempts;
            cfg.media_retry.delay_ms = 0;
            cfg.media_retry.max_delay_ms = 0;
            let output = directory.path().join("out.flac");
            // Exercise the CLI encoding path, which holds its stage slot across attempts.
            let result = cfg.encode_media(
                crate::media_backend::Encoding::Flac,
                &directory.path().join("in.wav"),
                &output,
                &[output.as_os_str().to_owned()],
            );
            assert_eq!(result.is_ok(), succeeds, "attempts {attempts}");
            assert_eq!(invocations(&count), attempts.min(3));
            assert_eq!(
                cfg.media_retries.load(Ordering::Relaxed),
                attempts.min(3) - 1
            );
            if succeeds {
                // A partial output from a failed attempt is never reused.
                assert_eq!(fs::read(&output).unwrap(), b"complete");
            } else {
                assert!(
                    matches!(result, Err(Error::Export(m)) if m.contains("temporarily unavailable"))
                );
            }
        }
        // A child killed by a signal is transient too; a deterministic failure is not retried.
        let directory = tempfile::tempdir().unwrap();
        let (script, count) = flaky_media_tool(directory.path(), 1, "kill -9 $$");
        let mut cfg = config(directory.path());
        cfg.ffmpeg = script;
        cfg.media_retry.attempts = 2;
        cfg.media_retry.delay_ms = 1;
        let output = directory.path().join("signal.wav");
        cfg.ffmpeg_to(&[output.as_os_str().to_owned()], &[&output])
            .unwrap();
        assert_eq!(invocations(&count), 2);
        for failure in [
            "echo 'Invalid data found when processing input' >&2; exit 1",
            "exit 1",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (script, count) = flaky_media_tool(directory.path(), 1, failure);
            let mut cfg = config(directory.path());
            cfg.ffmpeg = script;
            cfg.media_retry.attempts = 8;
            cfg.media_retry.delay_ms = 0;
            cfg.media_retry.max_delay_ms = 0;
            let output = directory.path().join("x.mp3");
            assert!(cfg
                .ffmpeg_to(&[output.as_os_str().to_owned()], &[&output])
                .is_err());
            assert_eq!(invocations(&count), 1);
            assert_eq!(cfg.media_retries.load(Ordering::Relaxed), 0);
        }
        // A missing executable is a permanent spawn failure.
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = config(directory.path());
        cfg.ffmpeg = directory.path().join("missing-executable");
        cfg.media_retry.attempts = 8;
        cfg.media_retry.delay_ms = 0;
        cfg.media_retry.max_delay_ms = 0;
        assert!(cfg.ffmpeg(&[]).is_err());
        assert_eq!(cfg.media_retries.load(Ordering::Relaxed), 0);
    }
    #[cfg(unix)]
    #[test]
    fn media_retry_backoff_releases_permits_and_stops_on_cancel_or_deadline() {
        use std::sync::{atomic::Ordering, Arc};
        let transient = "echo 'Connection reset by peer' >&2; exit 1";
        let directory = tempfile::tempdir().unwrap();
        let (script, count) = flaky_media_tool(directory.path(), 8, transient);
        let mut cfg = config(directory.path());
        cfg.ffmpeg = script.clone();
        cfg.media_concurrency = 1;
        cfg.media_retry.attempts = 3;
        cfg.media_retry.delay_ms = 10_000;
        cfg.media_retry.max_delay_ms = 10_000;
        let cfg = Arc::new(cfg);
        let worker_cfg = cfg.clone();
        let output = directory.path().join("backoff.mp4");
        let worker_output = output.clone();
        let worker = std::thread::spawn(move || {
            worker_cfg.ffmpeg_to(&[worker_output.as_os_str().to_owned()], &[&worker_output])
        });
        let started = std::time::Instant::now();
        while cfg.media_retries.load(Ordering::Relaxed) == 0 {
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // During backoff the media slot is free for other work and the partial output is gone.
        let other = std::sync::atomic::AtomicBool::new(false);
        let permit = cfg
            .media_gate
            .acquire(
                1,
                &other,
                std::time::Instant::now() + std::time::Duration::from_millis(500),
            )
            .unwrap();
        drop(permit);
        assert!(!output.exists());
        let cancelled = std::time::Instant::now();
        cfg.cancel.store(true, Ordering::Relaxed);
        assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
        assert!(cancelled.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(invocations(&count), 1);

        // Cancellation while a child runs kills it and is never retried.
        let directory = tempfile::tempdir().unwrap();
        let (script, count) = flaky_media_tool(
            directory.path(),
            8,
            "sleep 3; echo 'Resource temporarily unavailable' >&2; exit 1",
        );
        let mut cfg = config(directory.path());
        cfg.ffmpeg = script;
        cfg.media_retry.attempts = 8;
        cfg.media_retry.delay_ms = 0;
        cfg.media_retry.max_delay_ms = 0;
        let cfg = Arc::new(cfg);
        let worker_cfg = cfg.clone();
        let output = directory.path().join("running.mp4");
        let worker = std::thread::spawn(move || {
            worker_cfg.ffmpeg_to(&[output.as_os_str().to_owned()], &[&output])
        });
        let started = std::time::Instant::now();
        while invocations(&count) == 0 {
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        cfg.cancel.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(invocations(&count), 1);
        assert_eq!(cfg.media_retries.load(Ordering::Relaxed), 0);

        // A retry whose backoff would pass media_timeout_seconds is not attempted or waited for.
        let directory = tempfile::tempdir().unwrap();
        let (script, count) = flaky_media_tool(directory.path(), 8, transient);
        let mut cfg = config(directory.path());
        cfg.ffmpeg = script;
        cfg.media_timeout_seconds = 1;
        cfg.media_retry.attempts = 8;
        cfg.media_retry.delay_ms = 5_000;
        cfg.media_retry.max_delay_ms = 5_000;
        let output = directory.path().join("deadline.mp4");
        let started = std::time::Instant::now();
        assert!(cfg
            .ffmpeg_to(&[output.as_os_str().to_owned()], &[&output])
            .is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(900));
        assert_eq!(invocations(&count), 1);
        assert_eq!(cfg.media_retries.load(Ordering::Relaxed), 0);
    }
    #[cfg(unix)]
    #[test]
    fn media_process_gate_bounds_children_and_cancels_or_times_out_before_spawn() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("media-fixture");
        let log = directory.path().join("events");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho start >> '{}'\nsleep 0.1\necho end >> '{}'\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        for limit in [1, 2] {
            let mut cfg = config(directory.path());
            cfg.ffmpeg = script.clone();
            cfg.media_concurrency = limit;
            let cfg = std::sync::Arc::new(cfg);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let cfg = cfg.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        cfg.ffmpeg(&[])
                    })
                })
                .collect();
            barrier.wait();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
            let mut active = 0usize;
            let mut starts = 0;
            for event in fs::read_to_string(&log).unwrap().lines() {
                if event == "start" {
                    active += 1;
                    starts += 1;
                } else {
                    active -= 1;
                }
                assert!(active <= limit);
            }
            assert_eq!(starts, 8);
            assert_eq!(active, 0);
            fs::remove_file(&log).unwrap();
        }
        let mut cfg = config(directory.path());
        cfg.ffmpeg = script.clone();
        cfg.media_concurrency = 1;
        let cfg = std::sync::Arc::new(cfg);
        let gate = cfg.media_gate.clone();
        let hold = gate
            .acquire(
                1,
                &cfg.cancel,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        let child_cfg = cfg.clone();
        let worker = std::thread::spawn(move || child_cfg.ffmpeg(&[]));
        std::thread::sleep(std::time::Duration::from_millis(50));
        cfg.cancel.store(true, Ordering::Relaxed);
        assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
        assert!(!log.exists());
        drop(hold);
        let mut cfg = std::sync::Arc::try_unwrap(cfg).ok().unwrap();
        cfg.cancel.store(false, Ordering::Relaxed);
        cfg.media_timeout_seconds = 1;
        let hold = gate
            .acquire(
                1,
                &cfg.cancel,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        let started = std::time::Instant::now();
        assert!(cfg.ffmpeg(&[]).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!log.exists());
        drop(hold);
        cfg.ffmpeg = directory.path().join("missing-executable");
        assert!(cfg.ffmpeg(&[]).is_err());
        cfg.ffmpeg = script;
        cfg.ffmpeg(&[]).unwrap();
        assert_eq!(fs::read_to_string(log).unwrap(), "start\nend\n");
        cfg.media_concurrency = 0;
        assert!(cfg.validate().is_err());
        cfg.media_concurrency = 5;
        assert!(cfg.validate().is_err());
    }
    #[test]
    fn export_selection_and_format_configuration_fail_closed() {
        use crate::export_options::{ImageExport, Selection};
        let default = Selection::default();
        assert!(default.full());
        let selected: Selection = yaml_serde::from_str(
            "providers: [unity]\nunity_class_ids: [28, 213]\nembedded_audio: false",
        )
        .unwrap();
        selected.validate().unwrap();
        assert!(!selected.full());
        assert!(selected.provider(Provider::EncryptedBundle));
        assert!(!selected.provider(Provider::Cri));
        assert!(selected.class(28));
        assert!(!selected.class(114));
        for text in [
            "providers: [cri]\nunity_class_ids: [28]",
            "providers: [unity, unity]",
            "unity_class_ids: [0]",
            "unity_class_ids: [28, 28]",
        ] {
            assert!(yaml_serde::from_str::<Selection>(text)
                .unwrap()
                .validate()
                .is_err());
        }
        assert!(yaml_serde::from_str::<Selection>("providers: [sekai]").is_err());
        assert!(yaml_serde::from_str::<ImageExport>("format: jpeg\nquality: 90").is_err());
        assert!(yaml_serde::from_str::<ImageExport>(
            "format: jpeg\nquality: 0\nbackground: [255,255,255]"
        )
        .unwrap()
        .validate()
        .is_err());
        let cfg: ExportConfig =
            yaml_serde::from_str(include_str!("../export-config.example.yaml")).unwrap();
        assert!(cfg.selection.full());
        cfg.image.validate().unwrap();
    }

    // Synthetic v22 serialized Texture2D: Unity 2022.3, inline RGBA32, one mip.
    // Field ordering cross-checked against unity-rs-core 0.5.1's public parser/oracle fixtures.
    // No game data; fixed 16x32 two-color/alpha pixels make vertical orientation observable.
    fn synthetic_texture() -> (Vec<u8>, Vec<u8>) {
        fn ints(out: &mut Vec<u8>, values: &[i32]) {
            for value in values {
                out.extend(value.to_le_bytes());
            }
        }
        let pixels = [[255, 0, 0, 128].repeat(256), [0, 0, 255, 64].repeat(256)].concat();
        let mut body = Vec::new();
        ints(&mut body, &[4]);
        body.extend(b"test"); // aligned name
        ints(&mut body, &[0, 0, 16, 32, pixels.len() as i32, 0, 4, 1]);
        body.extend([1, 0, 0, 0]); // readable/preprocessed/mip-limit and padding
        ints(&mut body, &[0, 0, 0, 1, 2]); // mip-limit group, streaming flag+priority, image count, dimension
        body.extend([0; 24]); // GL settings
        ints(&mut body, &[0, 0, 0, pixels.len() as i32]); // lightmap/color space/platform blob/data length
        body.extend(&pixels);
        body.extend([0; 16]); // empty streaming offset/size/path
        let mut metadata = b"2022.3.62f1\0".to_vec();
        ints(&mut metadata, &[13]);
        metadata.push(0); // player target, no type tree
        ints(&mut metadata, &[1, 28]);
        metadata.push(0); // one Texture2D type
        metadata.extend((-1_i16).to_le_bytes());
        metadata.extend([0; 16]);
        ints(&mut metadata, &[1]); // one object
        while !(metadata.len() + 48).is_multiple_of(4) {
            metadata.push(0);
        }
        metadata.extend(7_i64.to_le_bytes());
        metadata.extend(0_i64.to_le_bytes());
        ints(&mut metadata, &[body.len() as i32, 0, 0, 0, 0]);
        metadata.push(0);
        let offset = (48 + metadata.len()).next_multiple_of(16);
        let mut bytes = vec![0; 48];
        bytes[8..12].copy_from_slice(&22_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        bytes[24..32].copy_from_slice(&((offset + body.len()) as u64).to_be_bytes());
        bytes[32..40].copy_from_slice(&(offset as u64).to_be_bytes());
        bytes.extend(metadata);
        bytes.resize(offset, 0);
        bytes.extend(body);
        (bytes, pixels)
    }
    fn synthetic_texture_array(
        streamed: bool,
        stripped: bool,
        graphics_format: i32,
    ) -> (Vec<u8>, [Vec<u8>; 2], Vec<u8>) {
        fn int(out: &mut Vec<u8>, n: i32) {
            out.extend(n.to_le_bytes());
        }
        fn string(out: &mut Vec<u8>, text: &str) {
            int(out, text.len() as i32);
            out.extend(text.as_bytes());
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        let layers = [
            vec![255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 64, 9, 8, 7, 6],
            vec![
                1, 2, 3, 4, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150,
            ],
        ];
        // Each layer contains mip0 followed by its own mip1; the latter must not become a layer.
        let payload = [
            layers[0].clone(),
            vec![7, 7, 7, 255],
            layers[1].clone(),
            vec![8, 8, 8, 255],
        ]
        .concat();
        let version = if stripped {
            "2023.2.0f1"
        } else {
            "2022.3.62f1"
        };
        let mut body = Vec::new();
        string(&mut body, "../../array");
        body.extend(vec![0; if stripped { 4 } else { 8 }]); // versioned Texture base, aligned
        for n in [1, graphics_format, 2, 2, 2, 2] {
            int(&mut body, n);
        }
        if stripped {
            int(&mut body, 1);
        }
        int(&mut body, payload.len() as i32);
        body.extend([0; 24]);
        int(&mut body, 0);
        body.extend([1, 0, 0, 0]);
        if streamed {
            int(&mut body, 0);
            body.extend(3_i64.to_le_bytes());
            int(&mut body, payload.len() as i32);
            string(&mut body, "array.resS");
        } else {
            int(&mut body, payload.len() as i32);
            body.extend(&payload);
        }
        let mut metadata = version.as_bytes().to_vec();
        metadata.push(0);
        int(&mut metadata, 13);
        metadata.push(0);
        int(&mut metadata, 1);
        int(&mut metadata, 187);
        metadata.push(0);
        metadata.extend((-1_i16).to_le_bytes());
        metadata.extend([0; 16]);
        int(&mut metadata, 1);
        while !(metadata.len() + 48).is_multiple_of(4) {
            metadata.push(0);
        }
        metadata.extend(41_i64.to_le_bytes());
        metadata.extend(0_i64.to_le_bytes());
        for n in [body.len() as i32, 0, 0, 0, 0] {
            int(&mut metadata, n);
        }
        metadata.push(0);
        let offset = (48 + metadata.len()).next_multiple_of(16);
        let mut file = vec![0; 48];
        file[8..12].copy_from_slice(&22_u32.to_be_bytes());
        file[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        file[24..32].copy_from_slice(&((offset + body.len()) as u64).to_be_bytes());
        file[32..40].copy_from_slice(&(offset as u64).to_be_bytes());
        file.extend(metadata);
        file.resize(offset, 0);
        file.extend(body);
        (file, layers, payload)
    }
    #[test]
    fn texture_arrays_export_every_mip_zero_layer_with_correct_stride_and_scope() {
        for streamed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let input = root.path().join("input");
            fs::create_dir_all(input.join("assets")).unwrap();
            let (file, layers, payload) = synthetic_texture_array(streamed, false, 4);
            fs::write(input.join("assets/array.assets"), &file).unwrap();
            let asset = crate::update::AssetReceipt {
                relative_path: "array.assets".into(),
                provider: Provider::UnityBundle,
                bytes: file.len() as u64,
                downloaded_sha256: hex::encode(Sha256::digest(&file)),
                stored_sha256: hex::encode(Sha256::digest(&file)),
                decrypted: false,
            };
            let dependencies = std::collections::BTreeSet::from(["array.resS".into()]);
            let mut cfg = config(root.path());
            cfg.retain_outputs = true;
            cfg.detected_cpus = 1;
            cfg.cpu.limit_stages = true;
            cfg.stage_limits.image = Some(1);
            cfg.stage_limits.wait_timeout_seconds = 1;
            cfg.set_service_cpu_gate(std::sync::Arc::default(), Some(1));
            if streamed {
                let out = root.path().join("missing");
                fs::create_dir(&out).unwrap();
                let report = cfg
                    .process_resource(&input, &out, &asset, 0, 0, None)
                    .unwrap();
                assert!(!report.errors.is_empty());
                assert_eq!(fs::read_dir(out).unwrap().count(), 0);
                fs::write(
                    input.join("assets/array.resS"),
                    [vec![1, 2, 3], payload, vec![0xff]].concat(),
                )
                .unwrap();
            }
            let mut first_bytes = 0;
            for policy in ["auto", "image", "image_archive"] {
                cfg.read_kinds =
                    yaml_serde::from_str(&format!("classes: {{187: {policy}}}")).unwrap();
                cfg.read_kinds.validate().unwrap();
                let out = root.path().join(policy);
                fs::create_dir(&out).unwrap();
                let report = cfg
                    .process_resource(
                        &input,
                        &out,
                        &asset,
                        0,
                        0,
                        streamed.then_some(&dependencies),
                    )
                    .unwrap();
                assert!(
                    report.errors.is_empty(),
                    "streamed={streamed} policy={policy}: {:?}",
                    report.errors
                );
                assert_eq!(report.outputs.len(), 2);
                assert_eq!(report.selected_objects, 1);
                first_bytes = report.outputs[0].bytes;
                for (index, record) in report.outputs.iter().enumerate() {
                    assert_eq!(record.path, format!("0_41_layer_{index:04}.png"));
                    assert_eq!(record.object.as_ref().unwrap().class_id, 187);
                    assert_eq!(record.object.as_ref().unwrap().path_id, 41);
                    let bytes = fs::read(out.join("00000").join(&record.path)).unwrap();
                    assert_eq!(record.sha256, hex::encode(Sha256::digest(&bytes)));
                    let mut decoder = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
                    let mut decoded = vec![0; decoder.output_buffer_size().unwrap()];
                    let info = decoder.next_frame(&mut decoded).unwrap();
                    assert_eq!((info.width, info.height), (2, 2));
                    let expected: Vec<u8> = layers[index]
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .rev()
                        .flatten()
                        .copied()
                        .collect();
                    assert_eq!(&decoded[..info.buffer_size()], expected);
                }
                assert_eq!(fs::read_dir(out.join("00000")).unwrap().count(), 2);
            }
            cfg.max_resource_output_bytes = first_bytes;
            let out = root.path().join("limited");
            fs::create_dir(&out).unwrap();
            let report = cfg
                .process_resource(
                    &input,
                    &out,
                    &asset,
                    0,
                    0,
                    streamed.then_some(&dependencies),
                )
                .unwrap();
            assert!(!report.errors.is_empty());
            assert_eq!(report.outputs.len(), 1);
            assert_eq!(fs::read_dir(out).unwrap().count(), 0);
        }
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for independent array rendition decoding"]
    fn texture_arrays_preserve_each_layer_across_image_renditions() {
        let root = tempfile::tempdir().unwrap();
        let (file, layers, _) = synthetic_texture_array(false, false, 4);
        fs::create_dir(root.path().join("assets")).unwrap();
        fs::write(root.path().join("assets/array.assets"), &file).unwrap();
        let asset = crate::update::AssetReceipt {
            relative_path: "array.assets".into(),
            provider: Provider::UnityBundle,
            bytes: file.len() as u64,
            downloaded_sha256: hex::encode(Sha256::digest(&file)),
            stored_sha256: hex::encode(Sha256::digest(&file)),
            decrypted: false,
        };
        let mut cfg = config(root.path());
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        cfg.retain_outputs = true;
        cfg.detected_cpus = 1;
        cfg.cpu.limit_stages = true;
        cfg.stage_limits.image = Some(1);
        cfg.set_service_cpu_gate(std::sync::Arc::default(), Some(1));
        cfg.image = yaml_serde::from_str("[{format: png}, {format: webp}]").unwrap();
        let out = root.path().join("out");
        fs::create_dir(&out).unwrap();
        let report = cfg
            .process_resource(root.path(), &out, &asset, 0, 0, None)
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.outputs.len(), 4);
        for (layer, pixels) in layers.iter().enumerate() {
            let expected: Vec<u8> = pixels
                .as_chunks::<8>()
                .0
                .iter()
                .rev()
                .flatten()
                .copied()
                .collect();
            for extension in ["png", "webp"] {
                let name = format!("0_41_layer_{layer:04}.{extension}");
                let record = report.outputs.iter().find(|r| r.path == name).unwrap();
                assert_eq!(record.object.as_ref().unwrap().class_id, 187);
                assert_eq!(record.object.as_ref().unwrap().path_id, 41);
                let path = out.join("00000").join(&name);
                assert_eq!(
                    record.sha256,
                    hex::encode(Sha256::digest(fs::read(&path).unwrap()))
                );
                let raw = root.path().join(format!("{name}.rgba"));
                cfg.ffmpeg(&[
                    "-i".into(),
                    path.into_os_string(),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    "rgba".into(),
                    raw.as_os_str().to_owned(),
                ])
                .unwrap();
                assert_eq!(fs::read(raw).unwrap(), expected);
            }
        }
    }
    #[test]
    fn texture_arrays_reject_unknown_formats_and_missing_mip_zero() {
        for (stripped, format) in [(true, 4), (false, 999999)] {
            let root = tempfile::tempdir().unwrap();
            let (file, _, _) = synthetic_texture_array(false, stripped, format);
            let input = root.path().join("array.assets");
            fs::write(&input, file).unwrap();
            let out = root.path().join("out");
            fs::create_dir(&out).unwrap();
            let cfg = config(root.path());
            let mut report = ResourceReport::default();
            cfg.unity(&input, &out, 0, &mut report, None, root.path())
                .unwrap();
            assert!(!report.errors.is_empty());
            assert!(report.outputs.is_empty());
            assert_eq!(fs::read_dir(out).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn raw_bundles_publish_verified_source_bytes_alone_or_with_decoded_images() {
        for only in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let input = root.path().join("input");
            fs::create_dir_all(input.join("assets/nested")).unwrap();
            let (texture, _) = synthetic_texture();
            let source = if only {
                b"opaque verified bundle fixture".to_vec()
            } else {
                texture
            };
            fs::write(input.join("assets/nested/a.bundle"), &source).unwrap();
            let asset = crate::update::AssetReceipt {
                relative_path: "nested/a.bundle".into(),
                provider: Provider::EncryptedBundle,
                bytes: source.len() as u64,
                downloaded_sha256: "c".repeat(64),
                stored_sha256: hex::encode(Sha256::digest(&source)),
                decrypted: true,
            };
            let mut cfg = config(root.path());
            cfg.retain_outputs = true;
            cfg.raw_bundles = Some(
                yaml_serde::from_str(&format!(
                    "mode: {}\ninclude: ['^nested/']\nexclude: ['debug']\noutput_prefix: bundles",
                    if only { "only" } else { "alongside" }
                ))
                .unwrap(),
            );
            cfg.validate().unwrap();
            let out = root.path().join("exports");
            fs::create_dir(&out).unwrap();
            let report = cfg
                .process_resource(&input, &out, &asset, 0, 0, None)
                .unwrap();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.outputs.len(), if only { 1 } else { 2 });
            let raw = report
                .outputs
                .iter()
                .find(|r| r.kind == "raw_bundle")
                .unwrap();
            assert_eq!(raw.sha256, asset.stored_sha256);
            assert!(raw.object.is_none());
            assert_eq!(
                fs::read(out.join("00000/bundles/nested/a.bundle")).unwrap(),
                source
            );
            let mut summary = ExportSummary {
                schema_version: 4,
                region: crate::region::Region::Jp,
                platform: "iOS".into(),
                complete: true,
                retained: true,
                input_files: 1,
                catalog_files: 1,
                succeeded: 1,
                output_files: report.outputs.len(),
                output_bytes: report.outputs.iter().map(|r| r.bytes).sum(),
                unity_objects: report.objects,
                selected_unity_objects: report.selected_objects,
                skipped_unity_objects: report.skipped_objects,
                catalog_sha256: "b".repeat(64),
                full_catalog: true,
                full_export: !only,
                raw_bundles: cfg.raw_bundles.clone(),
                ..Default::default()
            };
            for record in &report.outputs {
                *summary.payloads.entry(record.kind.clone()).or_default() += 1;
            }
            write_json(&out.join("summary.json"), &summary).unwrap();
            let mut journal = sonic_rs::to_vec(&report).unwrap();
            journal.push(b'\n');
            fs::write(out.join("resources.jsonl"), journal).unwrap();
            let verified = crate::export_verify::verify(&out, crate::region::Region::Jp)
                .await
                .unwrap();
            assert_eq!(verified.full_export, !only);
            let storage_root = root.path().join("published");
            let storage: crate::storage::Config = yaml_serde::from_str(&format!(
                "providers:\n  - name: local\n    backend:\n      type: local\n      directory: '{}'\n", storage_root.display()
            )).unwrap();
            let published = storage.run(&out, crate::region::Region::Jp).await.unwrap();
            assert_eq!(
                fs::read(
                    storage_root
                        .join(&published.providers[0].prefix)
                        .join("00000/bundles/nested/a.bundle")
                )
                .unwrap(),
                source
            );
            if only {
                summary.full_export = true;
                write_json(&out.join("summary.json"), &summary).unwrap();
                assert!(
                    crate::export_verify::verify(&out, crate::region::Region::Jp)
                        .await
                        .is_err()
                );
                summary.full_export = false;
            }
            summary.raw_bundles = None;
            write_json(&out.join("summary.json"), &summary).unwrap();
            assert!(
                crate::export_verify::verify(&out, crate::region::Region::Jp)
                    .await
                    .is_err()
            );
        }
    }
    #[test]
    fn raw_bundles_fail_without_partial_publication_on_corruption_limits_and_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        fs::create_dir_all(input.join("assets")).unwrap();
        let (source, _) = synthetic_texture();
        fs::write(input.join("assets/a.bundle"), &source).unwrap();
        let mut asset = crate::update::AssetReceipt {
            relative_path: "a.bundle".into(),
            provider: Provider::UnityBundle,
            bytes: source.len() as u64,
            downloaded_sha256: hex::encode(Sha256::digest(&source)),
            stored_sha256: hex::encode(Sha256::digest(&source)),
            decrypted: false,
        };
        for failure in ["hash", "limit", "later-image", "cancel"] {
            let mut cfg = config(root.path());
            cfg.retain_outputs = true;
            cfg.raw_bundles = Some(Default::default());
            if failure == "hash" {
                asset.stored_sha256 = "a".repeat(64)
            } else {
                asset.stored_sha256 = hex::encode(Sha256::digest(&source))
            }
            if failure == "limit" {
                cfg.max_resource_output_bytes = source.len() as u64 - 1
            }
            if failure == "later-image" {
                cfg.max_resource_output_bytes = source.len() as u64
            }
            if failure == "cancel" {
                cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            let out = root.path().join(failure);
            fs::create_dir(&out).unwrap();
            let r = cfg
                .process_resource(&input, &out, &asset, 0, 0, None)
                .unwrap();
            assert!(!r.errors.is_empty(), "{failure}");
            assert_eq!(fs::read_dir(out).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn cri_container_preservation_keeps_exact_bytes_and_rejects_decoded_completeness() {
        for (extension, bytes) in [
            ("acb", synthetic_acb(0x12345678)),
            ("usm", b"CRIDsynthetic-preserved-container".to_vec()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let input = root.path().join("input");
            fs::create_dir_all(input.join("assets")).unwrap();
            let name = format!("source.{extension}");
            fs::write(input.join("assets").join(&name), &bytes).unwrap();
            let asset = crate::update::AssetReceipt {
                relative_path: name,
                provider: Provider::Cri,
                bytes: bytes.len() as u64,
                downloaded_sha256: hex::encode(Sha256::digest(&bytes)),
                stored_sha256: hex::encode(Sha256::digest(&bytes)),
                decrypted: false,
            };
            let mut cfg = config(root.path());
            cfg.retain_outputs = true;
            cfg.cri = yaml_serde::from_str(&format!("{extension}: preserve")).unwrap();
            cfg.ffmpeg = root.path().join("must-not-be-invoked");
            let output = root.path().join("exports");
            fs::create_dir(&output).unwrap();
            let report = cfg
                .process_resource(&input, &output, &asset, 0, 0, None)
                .unwrap();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.outputs.len(), 1);
            let item = &report.outputs[0];
            assert_eq!(item.kind, format!("cri_{extension}_container"));
            assert!(item.object.is_none());
            assert_eq!(item.sha256, asset.stored_sha256);
            assert_eq!(
                fs::read(output.join("00000").join(&item.path)).unwrap(),
                bytes
            );
            let mut summary = ExportSummary {
                schema_version: 4,
                region: crate::region::Region::Jp,
                platform: "iOS".into(),
                complete: true,
                retained: true,
                input_files: 1,
                catalog_files: 1,
                succeeded: 1,
                output_files: 1,
                output_bytes: bytes.len() as u64,
                catalog_sha256: "b".repeat(64),
                full_catalog: true,
                full_export: false,
                cri: cfg.cri.clone(),
                payloads: BTreeMap::from([(item.kind.clone(), 1)]),
                ..Default::default()
            };
            write_json(&output.join("summary.json"), &summary).unwrap();
            let mut journal = sonic_rs::to_vec(&report).unwrap();
            journal.push(b'\n');
            fs::write(output.join("resources.jsonl"), journal).unwrap();
            crate::export_verify::verify(&output, crate::region::Region::Jp)
                .await
                .unwrap();
            summary.full_export = true;
            write_json(&output.join("summary.json"), &summary).unwrap();
            assert!(
                crate::export_verify::verify(&output, crate::region::Region::Jp)
                    .await
                    .is_err()
            );
            summary.full_export = false;
            summary.cri = Default::default();
            write_json(&output.join("summary.json"), &summary).unwrap();
            assert!(
                crate::export_verify::verify(&output, crate::region::Region::Jp)
                    .await
                    .is_err()
            );
            cfg.max_resource_output_bytes = bytes.len() as u64 - 1;
            let limited = root.path().join("limited");
            fs::create_dir(&limited).unwrap();
            let failed = cfg
                .process_resource(&input, &limited, &asset, 0, 0, None)
                .unwrap();
            assert!(!failed.errors.is_empty());
            assert_eq!(fs::read_dir(&limited).unwrap().count(), 0);
            cfg.max_resource_output_bytes = max_output();
            cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let cancelled = root.path().join("cancelled");
            fs::create_dir(&cancelled).unwrap();
            let failed = cfg
                .process_resource(&input, &cancelled, &asset, 0, 0, None)
                .unwrap();
            assert!(!failed.errors.is_empty());
            assert_eq!(fs::read_dir(cancelled).unwrap().count(), 0);
        }
        assert!(yaml_serde::from_str::<crate::export_options::CriExport>("acb: ignored").is_err());
        assert!(yaml_serde::from_str::<crate::export_options::CriExport>("hca: preserve").is_err());
    }
    #[test]
    fn preserved_embedded_acb_uses_its_parent_directory_and_shares_the_output_budget() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("object.acb");
        fs::create_dir(&nested).unwrap();
        let mut cfg = config(root.path());
        cfg.cri.acb = crate::export_options::ContainerMode::Preserve;
        cfg.ffmpeg = root.path().join("not-an-encoder");
        let bytes = synthetic_acb(0x12345678);
        let mut report = ResourceReport::default();
        cfg.acb(&bytes, &nested, 0, &mut report, root.path())
            .unwrap();
        assert_eq!(report.outputs[0].path, "object.acb/container.acb");
        cfg.max_resource_output_bytes = bytes.len() as u64;
        let second = root.path().join("other.acb");
        fs::create_dir(&second).unwrap();
        assert!(matches!(
            cfg.acb(&bytes, &second, 0, &mut report, root.path()),
            Err(Error::Size)
        ));
        assert_eq!(fs::read_dir(second).unwrap().count(), 0);
    }

    // Synthetic named-object records; payloads are opaque markers, not playable media.
    fn synthetic_unity_media(
        class: i32,
        external: bool,
        hostile: bool,
        modern_movie: bool,
    ) -> (Vec<u8>, Vec<u8>) {
        fn int(out: &mut Vec<u8>, n: i32) {
            out.extend(n.to_le_bytes());
        }
        fn string(out: &mut Vec<u8>, s: &str) {
            int(out, s.len() as i32);
            out.extend(s.as_bytes());
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        let payload = b"synthetic-encoded-media-payload".to_vec();
        let mut body = Vec::new();
        string(&mut body, "../../unsafe-name");
        let version = if class == 152 && !modern_movie {
            "2018.4.0f1"
        } else {
            "2022.3.62f1"
        };
        if class == 152 {
            body.extend([0; 8]); // texture fallback block, aligned
            body.extend([1, 0, 0, 0]);
            body.extend([0; 12]); // loop, null AudioClip PPtr
            int(&mut body, payload.len() as i32);
            body.extend(&payload);
        } else {
            if class == 83 {
                for n in [1, 1, 48000, 16, 0, 0, 0] {
                    int(&mut body, n);
                }
                body.extend([1, 0, 1, 0]);
            } else {
                string(
                    &mut body,
                    if hostile {
                        "../../source.bad;name"
                    } else {
                        "../../source.mp4"
                    },
                );
                for n in [0, 0, 16, 16, 1, 1] {
                    int(&mut body, n);
                }
                body.extend(30_f64.to_le_bytes());
                body.extend(1_u64.to_le_bytes());
                for _ in 0..5 {
                    int(&mut body, 0);
                } // format and four empty arrays
            }
            string(&mut body, if external { "media.resS" } else { "" });
            body.extend((if external { 2_u64 } else { 0 }).to_le_bytes());
            body.extend((payload.len() as u64).to_le_bytes());
            if class == 83 {
                int(&mut body, 1);
            } else {
                body.extend([0, 1]);
            }
            if !external {
                body.extend(&payload);
            }
        }
        let mut metadata = version.as_bytes().to_vec();
        metadata.push(0);
        int(&mut metadata, 13);
        metadata.push(0);
        int(&mut metadata, 1);
        int(&mut metadata, class);
        metadata.push(0);
        metadata.extend((-1_i16).to_le_bytes());
        metadata.extend([0; 16]);
        int(&mut metadata, 1);
        while !(metadata.len() + 48).is_multiple_of(4) {
            metadata.push(0);
        }
        metadata.extend(29_i64.to_le_bytes());
        metadata.extend(0_i64.to_le_bytes());
        for n in [body.len() as i32, 0, 0, 0, 0] {
            int(&mut metadata, n);
        }
        metadata.push(0);
        let offset = (48 + metadata.len()).next_multiple_of(16);
        let mut file = vec![0; 48];
        file[8..12].copy_from_slice(&22_u32.to_be_bytes());
        file[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        file[24..32].copy_from_slice(&((offset + body.len()) as u64).to_be_bytes());
        file[32..40].copy_from_slice(&(offset as u64).to_be_bytes());
        file.extend(metadata);
        file.resize(offset, 0);
        file.extend(body);
        (file, payload)
    }
    #[test]
    fn unity_media_extracts_inline_and_dependency_payloads_with_safe_object_names() {
        for (class, extension, kind, mode) in [
            (83, "fsb", "audio_raw", "audio"),
            (329, "mp4", "video_raw", "video"),
            (152, "ogv", "movie_ogv", "video"),
        ] {
            for external in [false, true]
                .into_iter()
                .filter(|external| class != 152 || !external)
            {
                let root = tempfile::tempdir().unwrap();
                let input = root.path().join("input");
                fs::create_dir_all(input.join("assets")).unwrap();
                let (file, payload) = synthetic_unity_media(class, external, false, false);
                fs::write(input.join("assets/media.assets"), &file).unwrap();
                let asset = crate::update::AssetReceipt {
                    relative_path: "media.assets".into(),
                    provider: Provider::UnityBundle,
                    bytes: file.len() as u64,
                    downloaded_sha256: hex::encode(Sha256::digest(&file)),
                    stored_sha256: hex::encode(Sha256::digest(&file)),
                    decrypted: false,
                };
                let mut cfg = config(root.path());
                cfg.retain_outputs = true;
                if external {
                    let missing = root.path().join("missing");
                    fs::create_dir(&missing).unwrap();
                    let report = cfg
                        .process_resource(&input, &missing, &asset, 0, 0, None)
                        .unwrap();
                    assert!(!report.errors.is_empty());
                    assert!(report.outputs.is_empty());
                    assert_eq!(fs::read_dir(&missing).unwrap().count(), 0);
                    // Raw object mode does not require its referenced media stream.
                    cfg.read_kinds.default = crate::read_policy::Kind::ObjectRaw;
                    let raw = root.path().join("raw");
                    fs::create_dir(&raw).unwrap();
                    let report = cfg
                        .process_resource(&input, &raw, &asset, 0, 0, None)
                        .unwrap();
                    assert!(report.errors.is_empty());
                    assert_eq!(report.outputs[0].kind, "raw_object");
                    cfg.read_kinds = Default::default();
                    fs::write(
                        input.join("assets/media.resS"),
                        [vec![0xaa, 0xbb], payload.clone()].concat(),
                    )
                    .unwrap();
                }
                let dependencies = std::collections::BTreeSet::from(["media.resS".into()]);
                for policy in ["auto", mode] {
                    cfg.read_kinds =
                        yaml_serde::from_str(&format!("classes: {{{class}: {policy}}}")).unwrap();
                    cfg.read_kinds.validate().unwrap();
                    let out = root.path().join(policy);
                    fs::create_dir(&out).unwrap();
                    let report = cfg
                        .process_resource(
                            &input,
                            &out,
                            &asset,
                            0,
                            0,
                            external.then_some(&dependencies),
                        )
                        .unwrap();
                    assert!(
                        report.errors.is_empty(),
                        "{class} external={external}: {:?}",
                        report.errors
                    );
                    assert_eq!(report.outputs.len(), 1);
                    let record = &report.outputs[0];
                    assert_eq!(record.kind, kind);
                    assert_eq!(record.path, format!("0_29.{extension}"));
                    assert_eq!(record.object.as_ref().unwrap().class_id, class);
                    assert_eq!(record.sha256, hex::encode(Sha256::digest(&payload)));
                    assert_eq!(
                        fs::read(out.join("00000").join(&record.path)).unwrap(),
                        payload
                    );
                }
                cfg.max_resource_output_bytes = payload.len() as u64 - 1;
                let out = root.path().join("limited");
                fs::create_dir(&out).unwrap();
                let report = cfg
                    .process_resource(
                        &input,
                        &out,
                        &asset,
                        0,
                        0,
                        external.then_some(&dependencies),
                    )
                    .unwrap();
                assert!(!report.errors.is_empty());
                assert_eq!(fs::read_dir(out).unwrap().count(), 0);
                if external {
                    cfg.max_resource_output_bytes = 1024 * 1024;
                    fs::write(input.join("assets/media.resS"), [0xaa, 0xbb]).unwrap();
                    let truncated = root.path().join("truncated");
                    fs::create_dir(&truncated).unwrap();
                    let report = cfg
                        .process_resource(&input, &truncated, &asset, 0, 0, Some(&dependencies))
                        .unwrap();
                    assert!(!report.errors.is_empty());
                    assert!(report.outputs.is_empty());
                    assert_eq!(fs::read_dir(truncated).unwrap().count(), 0);
                }
            }
        }
    }
    #[test]
    fn unity_media_rejects_unsafe_extensions_and_unsupported_modern_movie_layout() {
        for (class, hostile, modern) in [(329, true, false), (152, false, true)] {
            let root = tempfile::tempdir().unwrap();
            let (file, _) = synthetic_unity_media(class, false, hostile, modern);
            let input = root.path().join("media.assets");
            fs::write(&input, file).unwrap();
            let out = root.path().join("outputs");
            fs::create_dir(&out).unwrap();
            let cfg = config(root.path());
            let mut report = ResourceReport::default();
            cfg.unity(&input, &out, 0, &mut report, None, root.path())
                .unwrap();
            assert!(!report.errors.is_empty());
            assert!(report.outputs.is_empty());
            assert_eq!(fs::read_dir(out).unwrap().count(), 0);
        }
    }

    #[test]
    fn raw_representation_exports_exact_object_bytes_and_explicit_json_never_falls_back() {
        let root = tempfile::tempdir().unwrap();
        let (texture, _) = synthetic_texture();
        let offset = u64::from_be_bytes(texture[32..40].try_into().unwrap()) as usize;
        let input = root.path().join("texture.assets");
        fs::write(&input, &texture).unwrap();
        let mut cfg = config(root.path());
        cfg.read_kinds = yaml_serde::from_str("default: image\nclasses: {28: object_raw}").unwrap();
        let mut report = ResourceReport::default();
        cfg.unity(&input, root.path(), 0, &mut report, None, root.path())
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.outputs.len(), 1);
        assert_eq!(report.outputs[0].kind, "raw_object");
        assert_eq!(report.outputs[0].object.as_ref().unwrap().class_id, 28);
        assert_eq!(
            fs::read(root.path().join(&report.outputs[0].path)).unwrap(),
            texture[offset..]
        );
        for policy in ["default: typetree_json", "default: font"] {
            let out = tempfile::tempdir().unwrap();
            cfg.read_kinds = yaml_serde::from_str(policy).unwrap();
            let mut report = ResourceReport::default();
            cfg.unity(&input, out.path(), 0, &mut report, None, root.path())
                .unwrap();
            assert!(!report.errors.is_empty());
            assert!(report.outputs.is_empty());
            assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
        }
    }
    // Synthetic v22 file with an explicit two-node type tree and a single i32 value.
    fn synthetic_typed_object() -> Vec<u8> {
        fn i(out: &mut Vec<u8>, value: i32) {
            out.extend(value.to_le_bytes());
        }
        let mut metadata = b"2022.3.62f1\0".to_vec();
        i(&mut metadata, 13);
        metadata.push(1); // player, enabled type tree
        i(&mut metadata, 1);
        i(&mut metadata, 12345); // synthetic class with no built-in decoder
        metadata.push(0);
        metadata.extend((-1_i16).to_le_bytes());
        metadata.extend([0; 16]);
        let strings = b"TestObject\0Base\0int\0value\0";
        i(&mut metadata, 2);
        i(&mut metadata, strings.len() as i32);
        for (level, ty, name, index) in [(0u8, 0, 11, 0), (1, 16, 20, 1)] {
            metadata.extend(1_u16.to_le_bytes());
            metadata.extend([level, 0]);
            for n in [ty, name, 4, index, 0] {
                i(&mut metadata, n);
            }
            metadata.extend(0_u64.to_le_bytes());
        }
        metadata.extend(strings);
        i(&mut metadata, 0); // no type dependencies
        i(&mut metadata, 1);
        while !(metadata.len() + 48).is_multiple_of(4) {
            metadata.push(0);
        }
        metadata.extend(19_i64.to_le_bytes());
        metadata.extend(0_i64.to_le_bytes());
        for n in [4, 0, 0, 0, 0] {
            i(&mut metadata, n);
        }
        metadata.push(0);
        let offset = (48 + metadata.len()).next_multiple_of(16);
        let mut file = vec![0; 48];
        file[8..12].copy_from_slice(&22_u32.to_be_bytes());
        file[20..24].copy_from_slice(&(metadata.len() as u32).to_be_bytes());
        file[24..32].copy_from_slice(&((offset + 4) as u64).to_be_bytes());
        file[32..40].copy_from_slice(&(offset as u64).to_be_bytes());
        file.extend(metadata);
        file.resize(offset, 0);
        file.extend(42_i32.to_le_bytes());
        file
    }
    #[test]
    fn explicit_typetree_reads_real_serialized_metadata_and_preserves_object_identity() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("typed.assets");
        fs::write(&input, synthetic_typed_object()).unwrap();
        let mut cfg = config(root.path());
        cfg.read_kinds =
            yaml_serde::from_str("default: object_raw\nclasses: {12345: typetree_json}").unwrap();
        let mut report = ResourceReport::default();
        cfg.unity(&input, root.path(), 0, &mut report, None, root.path())
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.outputs.len(), 1);
        let record = &report.outputs[0];
        assert_eq!(record.kind, "typetree_json");
        assert_eq!(record.object.as_ref().unwrap().path_id, 19);
        let value: sonic_rs::Value =
            sonic_rs::from_slice(&fs::read(root.path().join(&record.path)).unwrap()).unwrap();
        use sonic_rs::JsonValueTrait;
        assert_eq!(value.get("value").and_then(|v| v.as_i64()), Some(42));
    }
    #[test]
    fn actual_texture_rendition_preserves_identity_and_rejects_aggregate_overflow() {
        let root = tempfile::tempdir().unwrap();
        let (texture, pixels) = synthetic_texture();
        let input = root.path().join("texture.assets");
        fs::write(&input, texture).unwrap();
        let mut cfg = config(root.path());
        cfg.read_kinds
            .classes
            .insert(28, crate::read_policy::Kind::Image);
        cfg.detected_cpus = 1;
        cfg.cpu.limit_stages = true;
        cfg.stage_limits.image = Some(1);
        cfg.stage_limits.wait_timeout_seconds = 1;
        cfg.set_service_cpu_gate(std::sync::Arc::default(), Some(1));
        cfg.image = yaml_serde::from_str("[{format: png, compression: best}]").unwrap();
        let mut report = ResourceReport::default();
        cfg.unity(&input, root.path(), 0, &mut report, None, root.path())
            .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.outputs.len(), 1);
        let record = &report.outputs[0];
        let object = record.object.as_ref().unwrap();
        assert_eq!((object.path_id, object.class_id), (7, 28));
        let bytes = fs::read(root.path().join(&record.path)).unwrap();
        assert_eq!(record.sha256, hex::encode(Sha256::digest(&bytes)));
        let mut decoder = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
        let mut decoded = vec![0; decoder.output_buffer_size().unwrap()];
        let info = decoder.next_frame(&mut decoded).unwrap();
        let expected: Vec<u8> = pixels
            .as_chunks::<{ 16 * 4 }>()
            .0
            .iter()
            .rev()
            .flatten()
            .copied()
            .collect();
        assert_eq!(&decoded[..info.buffer_size()], expected);
        // Another object's output shares this same resource budget.
        cfg.max_resource_output_bytes = record.bytes;
        let out = root.path().join("overflow");
        fs::create_dir(&out).unwrap();
        let mut failed = report.clone();
        cfg.unity(&input, &out, 0, &mut failed, None, root.path())
            .unwrap();
        assert!(!failed.errors.is_empty());
        assert_eq!(failed.outputs.len(), 1);
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for actual multi-rendition Texture2D verification"]
    fn all_image_rendition_sets_export_and_decode_with_independent_ffmpeg() {
        let root = tempfile::tempdir().unwrap();
        let (texture, pixels) = synthetic_texture();
        let input = root.path().join("texture.assets");
        fs::write(&input, &texture).unwrap();
        fs::create_dir(root.path().join("assets")).unwrap();
        fs::write(root.path().join("assets/texture.assets"), &texture).unwrap();
        let asset = crate::update::AssetReceipt {
            relative_path: "texture.assets".into(),
            provider: Provider::UnityBundle,
            bytes: texture.len() as u64,
            downloaded_sha256: hex::encode(Sha256::digest(&texture)),
            stored_sha256: hex::encode(Sha256::digest(&texture)),
            decrypted: false,
        };
        let mut cfg = config(root.path());
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        cfg.retain_outputs = true;
        let formats = [
            "{format: png, compression: best}",
            "{format: webp}",
            "{format: bmp}",
            "{format: tga}",
            "{format: jpeg, quality: 100, background: [255,255,255]}",
        ];
        let flipped: Vec<u8> = pixels
            .as_chunks::<{ 16 * 4 }>()
            .0
            .iter()
            .rev()
            .flatten()
            .copied()
            .collect();
        for mask in 1u32..32 {
            let choices: Vec<_> = formats
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, value)| *value)
                .collect();
            cfg.image = yaml_serde::from_str(&format!("[{}]", choices.join(","))).unwrap();
            let out = root.path().join(mask.to_string());
            fs::create_dir(&out).unwrap();
            let report = cfg
                .process_resource(root.path(), &out, &asset, 0, 0, None)
                .unwrap();
            let out = out.join("00000");
            assert!(report.errors.is_empty(), "mask {mask}: {:?}", report.errors);
            assert_eq!(report.outputs.len(), mask.count_ones() as usize);
            assert_eq!(report.selected_objects, 1);
            let mut names = std::collections::HashSet::new();
            for record in &report.outputs {
                assert!(names.insert(&record.path));
                assert_eq!(record.object.as_ref().unwrap().path_id, 7);
                let encoded = fs::read(out.join(&record.path)).unwrap();
                assert_eq!(record.bytes, encoded.len() as u64);
                assert_eq!(record.sha256, hex::encode(Sha256::digest(encoded)));
                let raw = root
                    .path()
                    .join(format!("decoded-{mask}-{}.rgba", record.path));
                cfg.ffmpeg(&[
                    "-i".into(),
                    out.join(&record.path).into_os_string(),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    "rgba".into(),
                    raw.as_os_str().to_owned(),
                ])
                .unwrap();
                let actual = fs::read(raw).unwrap();
                assert_eq!(actual.len(), flipped.len());
                if record.kind == "image_jpeg" {
                    for (pixel, source) in actual
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(flipped.as_chunks::<4>().0.iter())
                    {
                        assert_eq!(pixel[3], 255);
                        for c in 0..3 {
                            let expected = ((source[c] as u32 * source[3] as u32
                                + 255 * (255 - source[3] as u32)
                                + 127)
                                / 255) as u8;
                            assert!(pixel[c].abs_diff(expected) <= 4);
                        }
                    }
                } else {
                    assert_eq!(actual, flipped, "mask {mask}, {}", record.path);
                }
            }
            assert_eq!(fs::read_dir(out).unwrap().count(), choices.len());
        }
        // First rendition can succeed, but later overflow cannot become successful publication.
        cfg.image = yaml_serde::from_str("[{format: png}, {format: webp}]").unwrap();
        let image = unity_rs_core::texture::RgbaImage {
            width: 16,
            height: 32,
            pixels,
        };
        let first = crate::export_options::ImageExport::default()
            .encode(
                &image,
                unity_rs_core::image_export::ImageRowOrder::UnityDecoded,
                1024 * 1024,
            )
            .unwrap();
        cfg.max_resource_output_bytes = first.len() as u64;
        let out = root.path().join("partial");
        fs::create_dir(&out).unwrap();
        let report = cfg
            .process_resource(root.path(), &out, &asset, 0, 0, None)
            .unwrap();
        assert!(!report.errors.is_empty());
        assert_eq!(report.outputs.len(), 1);
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
        cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut cancelled = ResourceReport::default();
        assert!(matches!(
            cfg.image_outputs(
                &image,
                unity_rs_core::image_export::ImageRowOrder::Display,
                &out,
                "cancelled",
                &mut cancelled
            ),
            Err(Error::Cancelled)
        ));
        assert!(cancelled.outputs.is_empty());
    }

    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG for independent image/audio roundtrip verification"]
    fn configurable_images_preserve_rgba_orientation_and_flac_preserves_pcm() {
        use crate::export_options::{AudioExport, ImageExport};
        use unity_rs_core::{image_export::ImageRowOrder, texture::RgbaImage};
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.ffmpeg = std::env::var("SIRIUS_TEST_FFMPEG").unwrap().into();
        let pixels = vec![255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 0, 50, 60, 70, 90];
        let image = RgbaImage {
            width: 2,
            height: 2,
            pixels: pixels.clone(),
        };
        for (i, format) in [
            ImageExport::default(),
            ImageExport::Png {
                compression: crate::export_options::PngCompression::Default,
            },
            ImageExport::Png {
                compression: crate::export_options::PngCompression::Best,
            },
            ImageExport::Webp {},
            ImageExport::Bmp {},
            ImageExport::Tga {},
        ]
        .into_iter()
        .enumerate()
        {
            for (j, order) in [ImageRowOrder::Display, ImageRowOrder::UnityDecoded]
                .into_iter()
                .enumerate()
            {
                let bytes = format.encode(&image, order, 1024 * 1024).unwrap();
                let path = root
                    .path()
                    .join(format!("image-{i}-{j}{}", format.native().extension()));
                fs::write(&path, bytes).unwrap();
                let mut report = ResourceReport::default();
                cfg.record(
                    root.path(),
                    &path,
                    format.native().payload_kind(),
                    &mut report,
                )
                .unwrap();
                let raw = root.path().join(format!("rgba-{i}-{j}.raw"));
                cfg.ffmpeg(&[
                    "-i".into(),
                    path.into_os_string(),
                    "-f".into(),
                    "rawvideo".into(),
                    "-pix_fmt".into(),
                    "rgba".into(),
                    raw.as_os_str().to_owned(),
                ])
                .unwrap();
                let expected = if j == 0 {
                    pixels.clone()
                } else {
                    [pixels[8..].to_vec(), pixels[..8].to_vec()].concat()
                };
                assert_eq!(fs::read(raw).unwrap(), expected, "format {i} order {j}");
            }
            assert!(format.encode(&image, ImageRowOrder::Display, 1).is_err());
        }
        let format = ImageExport::Jpeg {
            quality: 100,
            background: [255, 255, 255],
        };
        let image = RgbaImage {
            width: 16,
            height: 16,
            pixels: [255, 0, 0, 128].repeat(256),
        };
        let jpeg = root.path().join("background.jpg");
        fs::write(
            &jpeg,
            format
                .encode(&image, ImageRowOrder::Display, 1024 * 1024)
                .unwrap(),
        )
        .unwrap();
        let raw = root.path().join("jpeg.raw");
        cfg.ffmpeg(&[
            "-i".into(),
            jpeg.into_os_string(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "rgba".into(),
            raw.as_os_str().to_owned(),
        ])
        .unwrap();
        for pixel in fs::read(raw).unwrap().as_chunks::<4>().0.iter() {
            for (actual, expected) in pixel.iter().zip([255u8, 127, 127, 255]) {
                assert!(actual.abs_diff(expected) <= 3);
            }
        }
        cfg.audio = AudioExport::Flac.into();
        let audio_dir = root.path().join("audio");
        fs::create_dir(&audio_dir).unwrap();
        let mut report = ResourceReport::default();
        cfg.acb(
            &synthetic_acb(0x12345678),
            &audio_dir,
            0x12345678,
            &mut report,
            &audio_dir,
        )
        .unwrap();
        assert!(audio_dir.join("00000.flac").is_file());
        assert!(!audio_dir.join("00000.wav").exists());
        assert_eq!(report.outputs[0].kind, "hca_flac");
        assert_eq!(report.outputs.len(), 2);
    }

    #[test]
    #[ignore = "requires SIRIUS_UNITY_SAMPLE pointing to a decrypted bundle with AssetBundle and other objects"]
    fn selected_unity_class_exports_only_selected_objects() {
        let sample = PathBuf::from(std::env::var("SIRIUS_UNITY_SAMPLE").unwrap());
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.selection.unity_class_ids = vec![142];
        cfg.selection.embedded_audio = false;
        let mut report = ResourceReport::default();
        cfg.unity(
            &sample,
            root.path(),
            0,
            &mut report,
            None,
            sample.parent().unwrap(),
        )
        .unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.selected_objects > 0);
        assert!(report.skipped_objects > 0);
        assert_eq!(
            report.objects,
            report.selected_objects + report.skipped_objects
        );
        assert_eq!(report.outputs.len(), report.selected_objects);
        assert!(report
            .outputs
            .iter()
            .all(|o| o.object.as_ref().unwrap().class_id == 142));
    }
    #[cfg(unix)]
    #[test]
    fn cancelled_or_timed_out_media_process_is_killed_and_reaped() {
        use std::{
            os::unix::fs::PermissionsExt,
            sync::atomic::Ordering,
            time::{Duration, Instant},
        };
        for cancel_requested in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let mut cfg = config(root.path());
            let executable = root.path().join("media-fixture");
            let pid_file = root.path().join("pid");
            // exec preserves the PID, so the test observes the actual long-running child.
            fs::write(
                &executable,
                format!(
                    "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
                    pid_file.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            cfg.ffmpeg = executable;
            // Parallel suites may delay shell startup past one second. Leave room
            // for its PID marker, while remaining far below the 60-second child.
            cfg.media_timeout_seconds = 5;
            let cancel = cfg.cancel.clone();
            let started = Instant::now();
            let worker = std::thread::spawn(move || cfg.ffmpeg(&[]));
            while !pid_file.exists() {
                if worker.is_finished() {
                    panic!(
                        "media child returned before readiness marker: {:?}",
                        worker.join().unwrap()
                    );
                }
                assert!(started.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(5));
            }
            if cancel_requested {
                cancel.store(true, Ordering::Relaxed);
            }
            assert!(worker.join().unwrap().is_err());
            assert!(started.elapsed() < Duration::from_secs(10));
            let pid: libc::pid_t = fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // Signal 0 probes existence without an external `kill` (absent in slim images);
            // a zombie would still exist, so this also proves the child was reaped.
            // SAFETY: signal 0 delivers nothing; it only checks the PID.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    // Synthesized 440 Hz PCM, encoded by cridecoder. No game bytes or real keys.
    pub(crate) fn synthetic_acb(key: u64) -> Vec<u8> {
        let samples: Vec<f32> = (0..4096)
            .map(|i| ((i as f32) * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.25)
            .collect();
        let mut encoder = cridecoder::HcaEncoder::new(
            cridecoder::HcaEncoderConfig::new(48000, 1).with_encryption(key),
        )
        .unwrap();
        let mut hca = Cursor::new(Vec::new());
        encoder.encode(&samples, &mut hca).unwrap();
        let mut builder = cridecoder::AcbBuilder::new();
        builder.add_track(cridecoder::TrackInput::new(
            "../../unsafe-cue",
            0,
            hca.into_inner(),
        ));
        let mut acb = Cursor::new(Vec::new());
        builder.build(&mut acb, None).unwrap();
        acb.into_inner()
    }
    fn cache_snapshot() -> crate::Snapshot {
        crate::Snapshot {
            region: Some(crate::region::Region::Jp),
            schema_version: 2,
            environment: "test".into(),
            platform: "iOS".into(),
            client_version: "1".into(),
            protocol_version: "1".into(),
            master_version: None,
            resource_version: "one".into(),
            platform_hash: "a".into(),
            effective_cdn_root: "https://example.invalid".into(),
            credential_ref: "unused".into(),
            observed_at: chrono::Utc::now(),
            source: "test".into(),
        }
    }
    #[test]
    fn resource_byte_budget_cancels_before_decode_and_releases_for_recovery() {
        use std::sync::{atomic::Ordering, Arc};
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        fs::create_dir_all(cfg.input.join("assets")).unwrap();
        fs::create_dir_all(&cfg.output).unwrap();
        let bytes = synthetic_acb(0x12345678);
        fs::write(cfg.input.join("assets/test.acb"), &bytes).unwrap();
        let asset = crate::update::AssetReceipt {
            relative_path: "test.acb".into(),
            provider: Provider::Cri,
            bytes: bytes.len() as u64,
            downloaded_sha256: hex::encode(Sha256::digest(&bytes)),
            stored_sha256: hex::encode(Sha256::digest(&bytes)),
            decrypted: false,
        };
        cfg.max_in_flight_bundle_bytes = 10;
        let budget = Arc::new(crate::resource_budget::Budget::new(10));
        cfg.set_service_resource_budget(Some(budget.clone()));
        let cfg = Arc::new(cfg);
        let hold = budget.acquire(1, &cfg.cancel).unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                cfg.process_resource(&cfg.input, &cfg.output, &asset, 0, 0x12345678, None)
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!worker.is_finished());
            assert_eq!(fs::read_dir(&cfg.output).unwrap().count(), 0);
            cfg.cancel.store(true, Ordering::Relaxed);
            assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
        });
        drop(hold);
        cfg.cancel.store(false, Ordering::Relaxed);
        let report = cfg
            .process_resource(&cfg.input, &cfg.output, &asset, 0, 0x12345678, None)
            .unwrap();
        assert!(report.errors.is_empty());
        assert!(!report.outputs.is_empty());
        let _permit = budget.acquire(10, &cfg.cancel).unwrap();
    }

    #[test]
    fn incremental_export_reuses_verified_audio_repairs_corruption_and_keeps_old_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut cfg = config(root);
        cfg.input = root.join("input");
        cfg.cache_directory = Some(root.join("cache"));
        cfg.retain_outputs = true;
        cfg.ffmpeg = std::env::current_exe().unwrap();
        fs::create_dir_all(cfg.input.join("assets")).unwrap();
        fs::create_dir(&cfg.output).unwrap();
        let bytes = synthetic_acb(0x12345678);
        fs::write(cfg.input.join("assets/test.acb"), &bytes).unwrap();
        let asset = crate::update::AssetReceipt {
            relative_path: "test.acb".into(),
            provider: Provider::Cri,
            bytes: bytes.len() as u64,
            downloaded_sha256: hex::encode(Sha256::digest(&bytes)),
            stored_sha256: hex::encode(Sha256::digest(&bytes)),
            decrypted: false,
        };
        let snapshot = cache_snapshot();
        let cache = cache::Cache::open(&cfg, &cfg.input, &cfg.output, &snapshot, 0x12345678)
            .unwrap()
            .unwrap();
        assert!(matches!(
            cache::Cache::open(&cfg, &cfg.input, &cfg.output, &snapshot, 0x12345678),
            Err(Error::Busy)
        ));
        let id = cache.identity(&asset, &BTreeMap::new()).unwrap();
        let run =
            |index| cfg.process_resource(&cfg.input, &cfg.output, &asset, index, 0x12345678, None);
        let first = cache.process(&id, &cfg.output, 0, || run(0)).unwrap();
        assert!(first.errors.is_empty() && !first.cache_hit);
        assert_eq!(first.outputs.len(), 2);
        let original = fs::read(cfg.output.join("00000/00000.wav")).unwrap();
        validate_wav(&original).unwrap();
        let second = cache
            .process(&id, &cfg.output, 1, || {
                panic!("cache hit must bypass decoding")
            })
            .unwrap();
        assert!(second.cache_hit);
        assert_eq!(second.output_directory, "00001");
        assert_eq!(
            fs::read(cfg.output.join("00001/00000.wav")).unwrap(),
            original
        );
        let entry = cfg.cache_directory.as_ref().unwrap().join(&id);
        fs::write(entry.join("data/00000.wav"), b"broken").unwrap();
        let repaired = cache.process(&id, &cfg.output, 2, || run(2)).unwrap();
        assert!(!repaired.cache_hit && repaired.errors.is_empty());
        assert_eq!(fs::read(entry.join("data/00000.wav")).unwrap(), original);
        // Publications are independent copies, not links into mutable cache storage.
        assert_eq!(
            fs::read(cfg.output.join("00000/00000.wav")).unwrap(),
            original
        );
        cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            cache.process(&id, &cfg.output, 3, || panic!()),
            Err(Error::Cancelled)
        ));
        assert!(!cfg.output.join("00003").exists());
        cfg.cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
        drop(cache);
        let cache = cache::Cache::open(&cfg, &cfg.input, &cfg.output, &snapshot, 0x12345678)
            .unwrap()
            .unwrap();
        assert!(
            cache
                .process(&id, &cfg.output, 4, || panic!())
                .unwrap()
                .cache_hit
        );
        let failed_id = cache
            .identity(
                &asset,
                &BTreeMap::from([("dependency".into(), "changed".into())]),
            )
            .unwrap();
        let failed = cache
            .process(&failed_id, &cfg.output, 5, || {
                Ok(ResourceReport {
                    errors: vec!["failed".into()],
                    ..Default::default()
                })
            })
            .unwrap();
        assert!(!failed.errors.is_empty());
        assert!(!cfg
            .cache_directory
            .as_ref()
            .unwrap()
            .join(failed_id)
            .exists());
        assert!(fs::read_dir(cfg.cache_directory.as_ref().unwrap())
            .unwrap()
            .all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pending")));
    }

    #[test]
    fn incremental_identity_tracks_content_dependencies_formats_regions_tools_and_key() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut cfg = config(root);
        cfg.input = root.join("input");
        cfg.cache_directory = Some(root.join("cache"));
        cfg.retain_outputs = true;
        cfg.ffmpeg = root.join("media-tool");
        fs::write(&cfg.ffmpeg, b"tool-one").unwrap();
        fs::create_dir(&cfg.input).unwrap();
        fs::create_dir(&cfg.output).unwrap();
        let mut snapshot = cache_snapshot();
        let mut asset = crate::update::AssetReceipt {
            relative_path: "test".into(),
            provider: Provider::UnityBundle,
            bytes: 1,
            downloaded_sha256: "download".into(),
            stored_sha256: "stored".into(),
            decrypted: false,
        };
        let id = |cfg: &ExportConfig,
                  snapshot: &crate::Snapshot,
                  asset: &crate::update::AssetReceipt,
                  deps: &BTreeMap<String, String>,
                  key| {
            cache::Cache::open(cfg, &cfg.input, &cfg.output, snapshot, key)
                .unwrap()
                .unwrap()
                .identity(asset, deps)
                .unwrap()
        };
        let deps = BTreeMap::new();
        let base = id(&cfg, &snapshot, &asset, &deps, 1);
        // A new catalog/resource version with identical source content remains reusable.
        snapshot.resource_version = "two".into();
        assert_eq!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 2));
        assert_ne!(
            base,
            id(
                &cfg,
                &snapshot,
                &asset,
                &BTreeMap::from([("atlas".into(), "hash".into())]),
                1
            )
        );
        asset.stored_sha256 = "changed".into();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        asset.stored_sha256 = "stored".into();
        snapshot.region = Some(crate::region::Region::En);
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        snapshot.region = Some(crate::region::Region::Jp);
        cfg.image = yaml_serde::from_str("format: png\ncompression: fast").unwrap();
        assert_eq!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        let mut compression_keys = std::collections::HashSet::new();
        compression_keys.insert(base.clone());
        for compression in ["default", "best"] {
            cfg.image =
                yaml_serde::from_str(&format!("format: png\ncompression: {compression}")).unwrap();
            assert!(compression_keys.insert(id(&cfg, &snapshot, &asset, &deps, 1)));
        }
        cfg.image = crate::export_options::ImageExport::Webp {}.into();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.image = Default::default();
        cfg.cri.acb = crate::export_options::ContainerMode::Preserve;
        let preserved_acb = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(base, preserved_acb);
        cfg.cri = Default::default();
        cfg.cri.usm = crate::export_options::ContainerMode::Preserve;
        let preserved_usm = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(base, preserved_usm);
        assert_ne!(preserved_acb, preserved_usm);
        cfg.cri = yaml_serde::from_str("acb: decode\nusm: decode").unwrap();
        assert_eq!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.raw_bundles = Some(Default::default());
        let raw_bundle_key = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(base, raw_bundle_key);
        cfg.raw_bundles.as_mut().unwrap().mode = crate::raw_bundles::Mode::Only;
        assert_ne!(raw_bundle_key, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.raw_bundles = None;
        cfg.read_kinds = yaml_serde::from_str("default: object_raw").unwrap();
        let raw = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(base, raw);
        cfg.read_kinds = yaml_serde::from_str("classes: {28: image, 114: typetree_json}").unwrap();
        let custom = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(raw, custom);
        assert_ne!(base, custom);
        cfg.read_kinds = yaml_serde::from_str("classes: {114: typetree_json, 28: image}").unwrap();
        assert_eq!(custom, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.read_kinds = Default::default();
        cfg.image = yaml_serde::from_str("[{format: png}]").unwrap();
        assert_eq!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.image = yaml_serde::from_str("[{format: png}, {format: webp}]").unwrap();
        let multiple = id(&cfg, &snapshot, &asset, &deps, 1);
        assert_ne!(base, multiple);
        cfg.image =
            yaml_serde::from_str("[{format: webp}, {format: png, compression: fast}]").unwrap();
        assert_eq!(multiple, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.image = Default::default();
        cfg.audio = crate::export_options::AudioExport::Flac.into();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.audio = crate::export_options::AudioExport::Mp3.into();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.audio = Default::default();
        cfg.video = crate::export_options::VideoExport::Mp4;
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.video = Default::default();
        cfg.selection.embedded_audio = false;
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.selection.embedded_audio = true;
        cfg.media_backend = crate::media_backend::Backend::Auto;
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.media_backend = crate::media_backend::Backend::Cli;
        cfg.cache_revision = "new-shared-libraries".into();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.cache_revision.clear();
        fs::write(&cfg.ffmpeg, b"tool-two").unwrap();
        assert_ne!(base, id(&cfg, &snapshot, &asset, &deps, 1));
        cfg.retain_outputs = false;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn shared_cpu_admission_bounds_native_and_media_work_and_releases_local_slots() {
        let root = tempfile::tempdir().unwrap();
        let shared = std::sync::Arc::new(crate::media_gate::Gate::default());
        let held_cancel = std::sync::atomic::AtomicBool::new(false);
        let held = shared
            .acquire(
                1,
                &held_cancel,
                std::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        let mut configs = Vec::new();
        for name in ["first", "second"] {
            let path = root.path().join(name);
            fs::create_dir(&path).unwrap();
            let mut cfg = config(&path);
            cfg.detected_cpus = 1;
            cfg.cpu.limit_stages = true;
            cfg.set_service_cpu_gate(shared.clone(), Some(1));
            configs.push((cfg, path));
        }
        let (cfg, path) = &configs[0];
        std::thread::scope(|scope| {
            let cancel = &cfg.cancel;
            scope.spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            assert!(matches!(
                cfg.acb(
                    &synthetic_acb(0),
                    path,
                    0,
                    &mut ResourceReport::default(),
                    path
                ),
                Err(Error::Cancelled)
            ));
        });
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
        cfg.cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // The nonexistent executable must never be reached while another job owns the CPU slot.
        assert!(
            matches!(configs[1].0.ffmpeg_deadline(&[], std::time::Instant::now() + std::time::Duration::from_millis(30), &[]), Err(Error::Export(message)) if message == "CPU stage admission timed out")
        );
        drop(held);
        for (cfg, path) in configs {
            let mut report = ResourceReport::default();
            cfg.acb(&synthetic_acb(0), &path, 0, &mut report, &path)
                .unwrap();
            assert_eq!(report.outputs.len(), 2);
            validate_wav(&fs::read(path.join("00000.wav")).unwrap()).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn encoder_stage_deadlines_prevent_spawn_and_recover_without_cross_stage_blocking() {
        use crate::{media_backend::Encoding, stage_limits::Stage};
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.stage_limits.auto_tune = true;
        cfg.detected_cpus = 2;
        cfg.cpu.budget_ratio = 0.5;
        cfg.stage_limits.audio_encode = Some(64);
        cfg.stage_limits.video_encode = Some(64);
        cfg.media_timeout_seconds = 1;
        cfg.ffmpeg = root.path().join("encoder");
        fs::write(
            &cfg.ffmpeg,
            "#!/bin/sh\nfor target do :; done\nprintf started > \"$target\"\n",
        )
        .unwrap();
        fs::set_permissions(&cfg.ffmpeg, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("started");
        let input = root.path().join("input");
        let output = root.path().join("output");
        for (encoding, stage) in [
            (Encoding::Flac, Stage::AudioEncode),
            (Encoding::Mp3, Stage::AudioEncode),
            (Encoding::Mp4, Stage::VideoEncode),
        ] {
            cfg.media_timeout_seconds = 1;
            let permit = cfg
                .stage_gates
                .acquire(&cfg.effective_stage_limits().unwrap(), stage, &cfg.cancel)
                .unwrap();
            let started = std::time::Instant::now();
            assert!(cfg
                .encode_media(
                    encoding,
                    &input,
                    &output,
                    &[marker.clone().into_os_string()]
                )
                .is_err());
            assert!(started.elapsed() < std::time::Duration::from_secs(3));
            assert!(!marker.exists());
            cfg.media_timeout_seconds = 5;
            let other = if matches!(stage, Stage::AudioEncode) {
                Encoding::Mp4
            } else {
                Encoding::Flac
            };
            cfg.encode_media(other, &input, &output, &[marker.clone().into_os_string()])
                .unwrap();
            fs::remove_file(&marker).unwrap();
            cfg.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(matches!(
                cfg.encode_media(
                    encoding,
                    &input,
                    &output,
                    &[marker.clone().into_os_string()]
                ),
                Err(Error::Cancelled)
            ));
            assert!(!marker.exists());
            cfg.cancel
                .store(false, std::sync::atomic::Ordering::Relaxed);
            drop(permit);
            cfg.encode_media(
                encoding,
                &input,
                &output,
                &[marker.clone().into_os_string()],
            )
            .unwrap();
            fs::remove_file(&marker).unwrap();
        }
    }
    #[test]
    fn acb_and_hca_stage_waits_cancel_before_output_and_release_nested_permits() {
        for (stage, automatic) in [
            crate::stage_limits::Stage::Acb,
            crate::stage_limits::Stage::Hca,
        ]
        .into_iter()
        .flat_map(|stage| [(stage, false), (stage, true)])
        {
            let root = tempfile::tempdir().unwrap();
            let mut cfg = config(root.path());
            cfg.stage_limits.auto_tune = automatic;
            cfg.detected_cpus = 2;
            cfg.cpu.budget_ratio = 0.5;
            cfg.stage_limits.acb = Some(if automatic { 64 } else { 1 });
            cfg.stage_limits.hca = Some(if automatic { 64 } else { 1 });
            let permit = cfg
                .stage_gates
                .acquire(&cfg.effective_stage_limits().unwrap(), stage, &cfg.cancel)
                .unwrap();
            let mut report = ResourceReport::default();
            std::thread::scope(|scope| {
                let cancel = &cfg.cancel;
                scope.spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                });
                assert!(matches!(
                    cfg.acb(&synthetic_acb(0), root.path(), 0, &mut report, root.path()),
                    Err(Error::Cancelled)
                ));
            });
            assert!(report.outputs.is_empty());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
            drop(permit);
            cfg.cancel
                .store(false, std::sync::atomic::Ordering::Relaxed);
            cfg.acb(&synthetic_acb(0), root.path(), 0, &mut report, root.path())
                .unwrap();
            validate_wav(&fs::read(root.path().join("00000.wav")).unwrap()).unwrap();
            assert_eq!(report.outputs.len(), 2);
        }
    }
    #[test]
    fn encrypted_acb_exports_pcm_and_keeps_cue_names_out_of_paths() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let mut report = ResourceReport::default();
        config
            .acb(
                &synthetic_acb(0x12345678),
                root.path(),
                0x12345678,
                &mut report,
                root.path(),
            )
            .unwrap();
        let wav = fs::read(root.path().join("00000.wav")).unwrap();
        validate_wav(&wav).unwrap();
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48000);
        assert_eq!(wav.len(), 44 + 4096 * 2);
        assert!(wav[44..].iter().any(|b| *b != 0));
        assert_eq!(report.outputs.len(), 2);
        assert_eq!(report.outputs[0].sha256, hex::encode(Sha256::digest(&wav)));
        assert_eq!(report.outputs[0].path, "00000.wav");
        assert!(fs::read_to_string(root.path().join("00000.cues.json"))
            .unwrap()
            .contains("unsafe-cue"));
    }
    #[test]
    fn wave_decode_obeys_budget_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        config.max_resource_output_bytes = 100;
        assert!(config
            .acb(
                &synthetic_acb(0),
                root.path(),
                0,
                &mut ResourceReport::default(),
                root.path()
            )
            .is_err());
        assert!(!root.path().join("00000.wav").exists());
    }
    #[test]
    fn wav_validation_rejects_truncation_and_bad_format() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        config
            .acb(
                &synthetic_acb(0),
                root.path(),
                0,
                &mut ResourceReport::default(),
                root.path(),
            )
            .unwrap();
        let bytes = fs::read(root.path().join("00000.wav")).unwrap();
        assert!(validate_wav(&bytes[..bytes.len() - 1]).is_err());
        let mut wrong = bytes.clone();
        wrong[32] = 0;
        assert!(validate_wav(&wrong).is_err());
        let mut wrong = bytes;
        wrong[20] = 3;
        assert!(validate_wav(&wrong).is_err());
    }
    #[test]
    fn output_validation_rejects_invalid_json_and_png() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let path = root.path().join("bad");
        fs::write(&path, b"not a json or png").unwrap();
        for kind in ["typetree_json", "image_png"] {
            let mut report = ResourceReport::default();
            assert!(config
                .record(root.path(), &path, kind, &mut report)
                .is_err());
            assert!(report.outputs.is_empty());
        }
    }
}

// Compatibility with pre-audio_codec USM headers. Keep chunk boundaries:
// each audio packet leaves its first 0x140 bytes unmasked and resets the mask.
fn legacy_adx_stream(bytes: &[u8], key: u64) -> Result<Vec<u8>, Error> {
    let mask = legacy_audio_mask(key);
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 32)
            .ok_or_else(|| err("truncated legacy USM header"))?;
        let size = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let head = u16::from_be_bytes(header[8..10].try_into().unwrap()) as usize;
        let pad = u16::from_be_bytes(header[10..12].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(size + 8)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| err("truncated legacy USM chunk"))?;
        if head < 24 || head + pad > size || header[12] != 0 {
            return Err(err("invalid legacy USM layout"));
        }
        if &header[..4] == b"@SFA" && header[15] & 3 == 0 {
            let start = result.len();
            result.extend_from_slice(&bytes[offset + 8 + head..end - pad]);
            if let Some(masked) = result.get_mut(start + 0x140..) {
                for (i, byte) in masked.iter_mut().enumerate() {
                    *byte ^= mask[i % 32];
                }
            }
        }
        offset = end;
    }
    Ok(result)
}

// Adapted from cridecoder 0.3.5 (MIT, Haruki Dev Team).
// See LICENSE-cridecoder for the retained notice.
fn legacy_audio_mask(key: u64) -> [u8; 32] {
    let key1 = (key & 0xFFFFFFFF) as u32;
    let key2 = ((key >> 32) & 0xFFFFFFFF) as u32;

    let mut t = [0u8; 32];
    t[0x00] = (key1 & 0xFF) as u8;
    t[0x01] = ((key1 >> 8) & 0xFF) as u8;
    t[0x02] = ((key1 >> 16) & 0xFF) as u8;
    t[0x03] = (((key1 >> 24) & 0xFF) as u8).wrapping_sub(0x34);
    t[0x04] = ((key2 & 0xF) as u8).wrapping_add(0xF9);
    t[0x05] = ((key2 >> 8) & 0xFF) as u8 ^ 0x13;
    t[0x06] = (((key2 >> 16) & 0xFF) as u8).wrapping_add(0x61);
    t[0x07] = t[0x00] ^ 0xFF;
    t[0x08] = (t[0x02] as u16 + t[0x01] as u16) as u8;
    t[0x09] = (t[0x01] as i16 - t[0x07] as i16) as u8;
    t[0x0A] = t[0x02] ^ 0xFF;
    t[0x0B] = t[0x01] ^ 0xFF;
    t[0x0C] = (t[0x0B] as u16 + t[0x09] as u16) as u8;
    t[0x0D] = (t[0x08] as i16 - t[0x03] as i16) as u8;
    t[0x0E] = t[0x0D] ^ 0xFF;
    t[0x0F] = (t[0x0A] as i16 - t[0x0B] as i16) as u8;
    t[0x10] = (t[0x08] as i16 - t[0x0F] as i16) as u8;
    t[0x11] = t[0x10] ^ t[0x07];
    t[0x12] = t[0x0F] ^ 0xFF;
    t[0x13] = t[0x03] ^ 0x10;
    t[0x14] = (t[0x04] as i16 - 0x32) as u8;
    t[0x15] = (t[0x05] as u16 + 0xED) as u8;
    t[0x16] = t[0x06] ^ 0xF3;
    t[0x17] = (t[0x13] as i16 - t[0x0F] as i16) as u8;
    t[0x18] = (t[0x15] as u16 + t[0x07] as u16) as u8;
    t[0x19] = (0x21i16 - t[0x13] as i16) as u8;
    t[0x1A] = t[0x14] ^ t[0x17];
    t[0x1B] = (t[0x16] as u16 + t[0x16] as u16) as u8;
    t[0x1C] = (t[0x17] as u16 + 0x44) as u8;
    t[0x1D] = (t[0x03] as u16 + t[0x04] as u16) as u8;
    t[0x1E] = (t[0x05] as i16 - t[0x16] as i16) as u8;
    t[0x1F] = t[0x1D] ^ t[0x13];

    let t2 = b"URUC";
    let mut amask = [0u8; 32];

    for (i, &ti) in t.iter().enumerate() {
        if i & 1 != 0 {
            amask[i] = t2[(i >> 1) & 3];
        } else {
            amask[i] = ti ^ 0xFF;
        }
    }

    amask
}

fn adx_layout(bytes: &[u8]) -> Result<(u8, u32, u32, usize), Error> {
    let invalid = || err("invalid or truncated ADX blocks");
    if bytes.len() < 24
        || bytes[..2] != [0x80, 0]
        || bytes[4..7] != [3, 18, 4]
        || !(1..=16).contains(&bytes[7])
    {
        return Err(invalid());
    }
    let header = u16::from_be_bytes(bytes[2..4].try_into().unwrap()) as usize + 4;
    if header < 24 || header > bytes.len() || &bytes[header - 6..header] != b"(c)CRI" {
        return Err(invalid());
    }
    let rate = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    let samples = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
    if rate == 0 || samples == 0 {
        return Err(invalid());
    }
    let channels = bytes[7];
    let length = (samples as usize)
        .div_ceil(32)
        .checked_mul(channels as usize * 18)
        .ok_or_else(invalid)?;
    let end = header
        .checked_add(length)
        .filter(|n| *n <= bytes.len())
        .ok_or_else(invalid)?;
    if bytes[header..end]
        .as_chunks::<18>()
        .0
        .iter()
        .any(|b| b[0] & 0x80 != 0)
    {
        return Err(invalid());
    }
    if end < bytes.len() && (bytes.len() - end < 4 || bytes[end..end + 2] != [0x80, 1]) {
        return Err(invalid());
    }
    Ok((channels, rate, samples, end))
}

// CRI alpha movies carry a second independently encrypted video track. The
// native codec accepts a single SFV track, so retain each track's exact chunk
// payloads and present ALP as SFV in an in-memory container. No frame is dropped.
type UsmVideoPair = (Vec<u8>, Vec<u8>);
fn split_alpha_usm(bytes: &[u8]) -> Result<Option<UsmVideoPair>, Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut alpha = false;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 32)
            .ok_or_else(|| err("truncated USM chunk header"))?;
        let size = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let header_size = u16::from_be_bytes(header[8..10].try_into().unwrap()) as usize;
        let padding = u16::from_be_bytes(header[10..12].try_into().unwrap()) as usize;
        if header_size < 24 || header_size + padding > size || header[12] != 0 {
            return Err(err("unsupported USM chunk layout or channel"));
        }
        let end = offset
            .checked_add(size + 8)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| err("truncated USM chunk"))?;
        let chunk = &bytes[offset..end];
        match &header[..4] {
            b"CRID" | b"@SFV" | b"@SFA" => {}
            b"@ALP" => alpha = true,
            _ => return Err(err("unsupported USM chunk type")),
        }
        chunks.push(chunk);
        offset = end;
    }
    if !alpha {
        return Ok(None);
    }
    let mut color = Vec::new();
    let mut alpha = Vec::new();
    for chunk in chunks {
        match &chunk[..4] {
            b"CRID" => {
                color.extend_from_slice(chunk);
                alpha.extend_from_slice(chunk);
            }
            b"@ALP" => {
                alpha.extend_from_slice(b"@SFV");
                alpha.extend_from_slice(&chunk[4..]);
            }
            _ => color.extend_from_slice(chunk),
        }
    }
    Ok(Some((color, alpha)))
}

fn empty_mesh(value: &sonic_rs::Value) -> bool {
    value
        .get("m_VertexData")
        .and_then(|v| v.get("m_VertexCount"))
        .and_then(|v| v.as_u64())
        == Some(0)
        && value
            .get("m_IndexBuffer")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty())
        && value
            .get("m_SubMeshes")
            .and_then(|v| v.as_array())
            .is_some_and(|a| {
                a.iter().all(|v| {
                    v.get("indexCount").and_then(|v| v.as_u64()) == Some(0)
                        && v.get("vertexCount").and_then(|v| v.as_u64()) == Some(0)
                })
            })
        && value
            .get("m_StreamData")
            .is_none_or(|v| v.get("size").and_then(|v| v.as_u64()) == Some(0))
}

#[cfg(test)]
mod format_tests {
    use super::*;
    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 32];
        bytes[..4].copy_from_slice(tag);
        bytes[4..8].copy_from_slice(&((payload.len() + 24) as u32).to_be_bytes());
        bytes[8..10].copy_from_slice(&24u16.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
    #[test]
    fn legacy_adx_mask_resets_per_packet_and_preserves_headers() {
        // Synthetic packets and fake key; compare with the native mask formula's
        // fixed first four bytes, including the alternating CRI constant bytes.
        let key = 0x12345678;
        let mask = legacy_audio_mask(key);
        assert_eq!(&mask[..4], &[0x87, b'U', 0xcb, b'R']);
        let payload = vec![0x42; 0x180];
        let mut header = chunk(b"@SFA", b"metadata must not be audio");
        header[15] = 1;
        let source = [header, chunk(b"@SFA", &payload), chunk(b"@SFA", &payload)].concat();
        let result = legacy_adx_stream(&source, key).unwrap();
        assert_eq!(result.len(), 2 * payload.len());
        assert_eq!(&result[..0x140], &payload[..0x140]);
        assert_eq!(&result[..payload.len()], &result[payload.len()..]);
        assert_eq!(&result[0x140..0x144], &[0xc5, 0x17, 0x89, 0x10]);
        assert!(legacy_adx_stream(&source[..source.len() - 1], key).is_err());
    }
    #[test]
    fn alpha_stream_adapter_preserves_every_encrypted_payload() {
        let header = chunk(b"CRID", b"synthetic header");
        let color = chunk(b"@SFV", b"color bytes");
        let alpha = chunk(b"@ALP", b"alpha bytes");
        let source = [header.clone(), color.clone(), alpha].concat();
        let (c, a) = split_alpha_usm(&source).unwrap().unwrap();
        assert_eq!(c, [header.clone(), color].concat());
        assert_eq!(a, [header, chunk(b"@SFV", b"alpha bytes")].concat());
        assert!(split_alpha_usm(&source[..source.len() - 1]).is_err());
        assert!(split_alpha_usm(&chunk(b"BAD!", b"payload")).is_err());
    }
    #[test]
    fn alpha_adapter_rejects_unhandled_multiple_channels() {
        let mut bytes = chunk(b"@SFV", b"synthetic");
        bytes[12] = 1;
        assert!(split_alpha_usm(&bytes).is_err());
    }
    fn adx_fixture() -> Vec<u8> {
        // Format 3 / 18-byte blocks / 4-bit PCM residuals; four silence blocks.
        let mut b = vec![0; 36 + 4 * 18];
        b[0] = 0x80;
        b[2..4].copy_from_slice(&32u16.to_be_bytes());
        b[4..8].copy_from_slice(&[3, 18, 4, 1]);
        b[8..12].copy_from_slice(&48000u32.to_be_bytes());
        b[12..16].copy_from_slice(&128u32.to_be_bytes());
        b[16..18].copy_from_slice(&500u16.to_be_bytes());
        b[18] = 4;
        b[30..36].copy_from_slice(b"(c)CRI");
        b.extend_from_slice(&[0x80, 1, 0, 14]);
        b.extend_from_slice(&[0; 14]);
        b
    }
    #[test]
    fn adx_counts_complete_blocks_and_rejects_premature_eof() {
        let b = adx_fixture();
        assert_eq!(adx_layout(&b).unwrap(), (1, 48000, 128, 108));
        assert!(adx_layout(&b[..107]).is_err());
        let mut bad = b.clone();
        bad[36] = 0x80;
        assert!(adx_layout(&bad).is_err());
        let mut bad = b;
        bad[108] = 0;
        assert!(adx_layout(&bad).is_err());
    }
    #[test]
    fn only_provably_empty_meshes_use_structured_empty_export() {
        let v:sonic_rs::Value=sonic_rs::from_str(r#"{"m_VertexData":{"m_VertexCount":0},"m_IndexBuffer":[],"m_SubMeshes":[{"indexCount":0,"vertexCount":0}],"m_StreamData":{"size":0}}"#).unwrap();
        assert!(empty_mesh(&v));
        let v: sonic_rs::Value = sonic_rs::from_str(
            r#"{"m_VertexData":{"m_VertexCount":0},"m_IndexBuffer":[1],"m_SubMeshes":[]}"#,
        )
        .unwrap();
        assert!(!empty_mesh(&v));
    }
    #[test]
    #[ignore = "requires SIRIUS_TEST_FFMPEG pointing to a local FFmpeg executable"]
    fn real_ffmpeg_decodes_short_final_adx_packet_without_losing_samples() {
        let root = tempfile::tempdir().unwrap();
        let config: ExportConfig = yaml_serde::from_str(&format!(
            "input: unused\noutput: unused\ncri_key_env: UNUSED\nffmpeg: {}\n",
            std::env::var("SIRIUS_TEST_FFMPEG").unwrap()
        ))
        .unwrap();
        let path = root.path().join("audio.wav");
        config.adx(&adx_fixture(), &path).unwrap();
        validate_wav(&fs::read(path).unwrap()).unwrap();
    }
}

struct UsmKey {
    value: u64,
    masked: bool,
}
struct JsonBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for JsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(std::io::Error::other("JSON output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
