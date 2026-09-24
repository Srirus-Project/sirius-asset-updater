//! Offline, per-resource export. A successful download is not an export receipt.
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub input: PathBuf,
    #[serde(default)]
    pub paths: Vec<String>,
    pub output: PathBuf,
    #[serde(default)]
    pub retain_outputs: bool,
    pub cri_key_env: String,
    #[serde(default)]
    pub split_acb_xor_env: Option<String>,
    #[serde(default = "default_workers")]
    pub concurrency: usize,
    #[serde(default = "media_timeout")]
    pub media_timeout_seconds: u64,
    #[serde(skip)]
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub ffmpeg: PathBuf,
    #[serde(default = "max_output")]
    pub max_resource_output_bytes: u64,
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
    pub schema_version: u8,
    pub region: crate::region::Region,
    pub platform: String,
    pub complete: bool,
    pub input_files: usize,
    pub catalog_files: usize,
    pub unity_objects: usize,
    pub catalog_sha256: String,
    pub succeeded: usize,
    pub failed: usize,
    pub output_files: usize,
    pub output_bytes: u64,
    pub payloads: BTreeMap<String, usize>,
    pub retained: bool,
}
#[derive(Serialize)]
struct OutputRecord {
    path: String,
    kind: String,
    bytes: u64,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    object: Option<ObjectIdentity>,
}
#[derive(Clone, Serialize)]
struct ObjectIdentity {
    source_file: String,
    path_id: i64,
    class_id: i32,
    name: Option<String>,
    container: Option<String>,
}
#[derive(Default, Serialize)]
struct ResourceReport {
    source: String,
    output_directory: String,
    source_sha256: String,
    objects: usize,
    outputs: Vec<OutputRecord>,
    errors: Vec<String>,
}
fn err(e: impl std::fmt::Display) -> Error {
    Error::Export(e.to_string())
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    fs::write(path, sonic_rs::to_vec_pretty(value).map_err(err)?).map_err(err)
}
impl ExportConfig {
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
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
        if !(1..=3600).contains(&self.media_timeout_seconds)
            || !(1..=4).contains(&self.concurrency)
            || self.max_resource_output_bytes == 0
            || self.max_resource_output_bytes > 16 * 1024 * 1024 * 1024
        {
            return Err(Error::Config);
        }
        let key = std::env::var(&self.cri_key_env)
            .map_err(|_| Error::Secret)?
            .parse::<u64>()
            .map_err(|_| Error::Secret)?;
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
        if !Command::new(&self.ffmpeg)
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
        let mut assets = receipt.update.ok_or(Error::Verification)?.assets;
        let catalog_files = assets.len();
        if !self.paths.is_empty() {
            let selected: std::collections::BTreeSet<_> = self.paths.iter().collect();
            assets.retain(|a| selected.contains(&a.relative_path));
            if assets.len() != selected.len() {
                return Err(Error::AssetPath);
            }
        }
        let mut summary = ExportSummary {
            schema_version: 2,
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
        std::thread::scope(|scope| -> Result<(), Error> {
            let (send, recv) = std::sync::mpsc::sync_channel(self.concurrency);
            for _ in 0..self.concurrency {
                let send = send.clone();
                let assets = &assets;
                let next = &next;
                let input = &input;
                let dependencies = &dependencies;
                let root = &root;
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
                            self.process_resource(
                                input,
                                root,
                                asset,
                                i,
                                key,
                                dependencies.get(&asset.relative_path),
                            )
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
                } else {
                    summary.failed += 1;
                }
                summary.unity_objects += report.objects;
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
                    eprintln!(
                        "export {}/{} ok={} failed={} outputs={}",
                        completed,
                        assets.len(),
                        summary.succeeded,
                        summary.failed,
                        summary.output_files
                    );
                }
                write_json(&root.join("summary.json"), &summary)?;
            }
            Ok(())
        })?;
        summary.complete = summary.failed == 0 && summary.succeeded == summary.input_files;
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
        let result = match asset.provider {
            Provider::EncryptedBundle | Provider::UnityBundle => self.unity(
                &path,
                work.path(),
                key,
                &mut report,
                dependencies,
                &input.join("assets"),
            ),
            Provider::Cri => self.cri(&path, work.path(), key, &mut report),
        };
        if let Err(error) = result {
            report.errors.push(error.to_string());
        }
        if report.errors.is_empty() && self.retain_outputs {
            fs::rename(work.path(), root.join(format!("{i:05}"))).map_err(err)?;
        }
        Ok(report)
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
            image_export::{
                write_rgba_image_with_options, ImageEncodeOptions, ImageFormat, ImageRowOrder,
                PngCompression,
            },
            sprite::SpriteReadLimits,
            studio::Studio,
            texture::TextureReadLimits,
        };
        let mut studio = Studio::open(path).map_err(err)?;
        let own_files: std::collections::BTreeSet<_> =
            studio.files().map(|f| f.path().to_string()).collect();
        // Atlas references are on logical catalog locations, not bundle nodes.
        if studio.objects().any(|o| o.class_id() == 213) {
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
                studio = Studio::open_regions(regions).map_err(err)?;
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
            let stem = format!("{}_{}", object.file_index(), object.path_id());
            let output_start = report.outputs.len();
            let result = (|| -> Result<(), Error> {
                let limit = self.max_resource_output_bytes.min(512 * 1024 * 1024);
                let (data, extension, kind) = match object.class_id() {
                    28 | 213 => {
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
                        let mut png = Vec::new();
                        write_rgba_image_with_options(
                            &image,
                            ImageFormat::Png,
                            if object.class_id() == 28 {
                                ImageRowOrder::UnityDecoded
                            } else {
                                ImageRowOrder::Display
                            },
                            &ImageEncodeOptions {
                                png_compression: PngCompression::Fast,
                                maximum_output_bytes: limit,
                                ..ImageEncodeOptions::default()
                            },
                            &mut png,
                        )
                        .map_err(err)?;
                        (png, "png", "image_png")
                    }
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
                                if reason.contains("Mesh has no vertices") =>
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
                        Err(unity_rs_core::Error::Unsupported(_)) => (
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
                if object.class_id() == 114 && kind == "typetree_json" {
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
    fn acb(
        &self,
        bytes: &[u8],
        output: &Path,
        key: u64,
        report: &mut ResourceReport,
        root: &Path,
    ) -> Result<(), Error> {
        use cridecoder::acb::{AfsArchive, TrackList, UtfTable};
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
        for (i, entry) in entries.into_iter().enumerate() {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
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
            self.media_check(&target)?;
            self.record(root, &target, "hca_wav", report)?;
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
        if let Some((color, alpha)) = split_alpha_usm(&bytes)? {
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
                    self.record(report_root, &target, "adx_wav", report)?;
                    audio = Some(target);
                }
                "hca" => {
                    if audio.is_some() {
                        return Err(err("multiple audio streams are unsupported"));
                    }
                    let target = output.join(format!("{i:05}.wav"));
                    let mut decoder = cridecoder::HcaDecoder::from_reader(Cursor::new(stream.data))
                        .map_err(err)?;
                    decoder.set_encryption_key(crypto.value, 0);
                    decoder
                        .decode_to_wav(&mut fs::File::create(&target).map_err(err)?)
                        .map_err(err)?;
                    self.media_check(&target)?;
                    self.record(report_root, &target, "hca_wav", report)?;
                    audio = Some(target);
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
        self.ffmpeg(&args)?;
        let progress = tempfile::NamedTempFile::new_in(output).map_err(err)?;
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
        self.record(report_root, &movie, "usm_mkv", report)?;
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
        self.ffmpeg(&[
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
        ])?;
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
    fn ffmpeg(&self, args: &[std::ffi::OsString]) -> Result<(), Error> {
        let stderr = tempfile::tempfile().map_err(err)?;
        let mut child = Command::new(&self.ffmpeg)
            .env_remove(&self.cri_key_env)
            .args(["-v", "error", "-nostdin"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(stderr.try_clone().map_err(err)?)
            .spawn()
            .map_err(err)?;
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(err)? {
                break status;
            }
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed)
                || started.elapsed().as_secs() >= self.media_timeout_seconds
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err("media decoder cancelled or timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        use std::io::{Read, Seek, SeekFrom};
        let mut stderr = stderr;
        stderr.seek(SeekFrom::Start(0)).map_err(err)?;
        let mut diagnostic = String::new();
        stderr
            .take(8192)
            .read_to_string(&mut diagnostic)
            .map_err(err)?;
        if !status.success() || !diagnostic.is_empty() {
            return Err(err(format!("media decode: {status}: {diagnostic}")));
        }
        Ok(())
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
        report.outputs.push(OutputRecord {
            path: path
                .strip_prefix(root)
                .map_err(err)?
                .to_string_lossy()
                .into_owned(),
            kind: kind.into(),
            bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            object: None,
        });
        Ok(())
    }
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
mod tests {
    use super::*;
    fn config(root: &Path) -> ExportConfig {
        ExportConfig {
            input: root.into(),
            paths: Vec::new(),
            output: root.join("out"),
            retain_outputs: false,
            cri_key_env: "UNUSED_TEST_KEY".into(),
            split_acb_xor_env: None,
            concurrency: 1,
            media_timeout_seconds: 120,
            cancel: Default::default(),
            ffmpeg: "unused".into(),
            max_resource_output_bytes: 16 * 1024 * 1024,
        }
    }
    // Synthesized 440 Hz PCM, encoded by cridecoder. No game bytes or real keys.
    fn synthetic_acb(key: u64) -> Vec<u8> {
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
