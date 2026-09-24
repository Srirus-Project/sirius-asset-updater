//! Independent verification of retained export receipts before publication.
use crate::{
    export::{ExportSummary, ResourceReport},
    region::Region,
    Error,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const MAX_SUMMARY: u64 = 1024 * 1024;
const MAX_REPORT: u64 = 16 * 1024 * 1024;
const MAX_RESOURCES: usize = 1_000_000;

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u8,
    pub region: Region,
    pub resources_verified: usize,
    pub files_verified: usize,
    pub bytes_verified: u64,
    /// Scope declarations from the export receipt, not a reinspection of the source catalog.
    pub full_catalog: bool,
    pub full_export: bool,
    pub summary_sha256: String,
    pub journal_sha256: String,
}

/// A temporary, verified allowlist for a subsequent storage publication.
/// Each JSON line is an Object; includes the two receipt files.
/// The caller must keep the source directory immutable through upload and read-back.
pub struct VerifiedExport {
    pub report: Report,
    pub inventory: tempfile::NamedTempFile,
}
#[derive(Serialize)]
pub struct Object {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn safe_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && !s.contains(['\\', ':', '\0'])
        && !s.chars().any(char::is_control)
        && s.split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
async fn regular(path: &Path, directory: bool) -> Result<(), Error> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| Error::Verification)?;
    if meta.file_type().is_symlink()
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return Err(Error::Verification);
    }
    Ok(())
}
async fn file_hash(path: &Path, expected: u64) -> Result<String, Error> {
    regular(path, false).await?;
    let mut file = tokio::fs::File::open(path).await.map_err(|_| Error::Io)?;
    let before = file.metadata().await.map_err(|_| Error::Io)?;
    if before.len() != expected {
        return Err(Error::Verification);
    }
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).await.map_err(|_| Error::Io)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).ok_or(Error::Verification)?;
        if count > expected {
            return Err(Error::Verification);
        }
        hasher.update(&buffer[..n]);
    }
    let after = file.metadata().await.map_err(|_| Error::Io)?;
    if count != expected
        || after.len() != expected
        || before.modified().ok() != after.modified().ok()
    {
        return Err(Error::Verification);
    }
    Ok(hex::encode(hasher.finalize()))
}
async fn append(file: &mut tokio::fs::File, object: &Object) -> Result<(), Error> {
    let mut bytes = sonic_rs::to_vec(object).map_err(|_| Error::Verification)?;
    bytes.push(b'\n');
    file.write_all(&bytes).await.map_err(|_| Error::Io)
}
/// Reject links, devices and unlisted files before reading any resource payload.
async fn exact_tree(root: &Path, expected: &HashSet<String>) -> Result<(), Error> {
    regular(root, true).await?;
    let mut pending = vec![PathBuf::new()];
    let mut count = 0_usize;
    let mut directories = 0_usize;
    while let Some(relative) = pending.pop() {
        let mut entries = tokio::fs::read_dir(root.join(&relative))
            .await
            .map_err(|_| Error::Io)?;
        while let Some(entry) = entries.next_entry().await.map_err(|_| Error::Io)? {
            let path = relative.join(entry.file_name());
            let kind = entry.file_type().await.map_err(|_| Error::Io)?;
            if kind.is_dir() {
                directories += 1;
                // Every directory must be an ancestor of a declared file. No unrelated trees.
                let prefix = format!(
                    "{}/",
                    path.to_str().ok_or(Error::Verification)?.replace('\\', "/")
                );
                if directories > MAX_RESOURCES || !expected.iter().any(|p| p.starts_with(&prefix)) {
                    return Err(Error::Verification);
                }
                pending.push(path);
            } else if kind.is_file() {
                let normalized = path.to_str().ok_or(Error::Verification)?.replace('\\', "/");
                if !expected.contains(&normalized) {
                    return Err(Error::Verification);
                }
                count += 1;
            } else {
                return Err(Error::Verification);
            }
        }
    }
    if count != expected.len() {
        return Err(Error::Verification);
    }
    Ok(())
}

pub async fn verify(directory: &Path, region: Region) -> Result<Report, Error> {
    Ok(prepare(directory, region).await?.report)
}

pub async fn prepare(directory: &Path, region: Region) -> Result<VerifiedExport, Error> {
    if region == Region::Cn {
        return Err(Error::ReservedRegion);
    }
    regular(directory, true).await?;
    let summary_path = directory.join("summary.json");
    regular(&summary_path, false).await?;
    let mut summary_bytes = Vec::new();
    tokio::fs::File::open(&summary_path)
        .await
        .map_err(|_| Error::Io)?
        .take(MAX_SUMMARY + 1)
        .read_to_end(&mut summary_bytes)
        .await
        .map_err(|_| Error::Io)?;
    if summary_bytes.len() as u64 > MAX_SUMMARY {
        return Err(Error::Verification);
    }
    let summary: ExportSummary =
        sonic_rs::from_slice(&summary_bytes).map_err(|_| Error::Verification)?;
    if summary.schema_version != 4
        || summary.region != region
        || !summary.complete
        || !summary.retained
        || summary.failed != 0
        || summary.succeeded != summary.input_files
        || summary.input_files == 0
        || summary.input_files > MAX_RESOURCES
        || summary.input_files > summary.catalog_files
        || summary.output_files == 0
        || !valid_digest(&summary.catalog_sha256)
        || (summary.full_export && !summary.full_catalog)
        || (summary.full_catalog && summary.input_files != summary.catalog_files)
    {
        return Err(Error::Verification);
    }
    let inventory = tempfile::NamedTempFile::new().map_err(|_| Error::Io)?;
    let mut target = tokio::fs::File::from_std(inventory.reopen().map_err(|_| Error::Io)?);
    let journal_path = directory.join("resources.jsonl");
    regular(&journal_path, false).await?;
    let mut journal = BufReader::new(
        tokio::fs::File::open(&journal_path)
            .await
            .map_err(|_| Error::Io)?,
    );
    let mut journal_hash = Sha256::new();
    let mut journal_bytes = 0_u64;
    let mut directories = HashSet::new();
    let mut sources = HashSet::new();
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    let mut objects = 0_usize;
    let mut selected = 0_usize;
    let mut skipped = 0_usize;
    let mut hits = 0_usize;
    let mut kinds = BTreeMap::<String, usize>::new();
    loop {
        let mut line = Vec::new();
        let size = (&mut journal)
            .take(MAX_REPORT + 1)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| Error::Io)?;
        if size == 0 {
            break;
        }
        if size as u64 > MAX_REPORT
            || line.last() != Some(&b'\n')
            || directories.len() >= summary.input_files
        {
            return Err(Error::Verification);
        }
        journal_hash.update(&line);
        journal_bytes = journal_bytes
            .checked_add(size as u64)
            .ok_or(Error::Verification)?;
        let resource: ResourceReport =
            sonic_rs::from_slice(&line).map_err(|_| Error::Verification)?;
        let name = &resource.output_directory;
        let index = name.parse::<usize>().map_err(|_| Error::Verification)?;
        if name != &format!("{index:05}")
            || index >= summary.input_files
            || !directories.insert(name.clone())
            || !sources.insert(resource.source.clone())
            || !safe_path(&resource.source)
            || !valid_digest(&resource.source_sha256)
            || !resource.errors.is_empty()
        {
            return Err(Error::Verification);
        }
        let mut paths = HashSet::new();
        for item in &resource.outputs {
            if !safe_path(&item.path)
                || !paths.insert(item.path.clone())
                || !valid_digest(&item.sha256)
            {
                return Err(Error::Verification);
            }
        }
        exact_tree(&directory.join(name), &paths).await?;
        for item in resource.outputs {
            let path = format!("{name}/{}", item.path);
            if file_hash(&directory.join(&path), item.bytes).await? != item.sha256 {
                return Err(Error::Verification);
            }
            files = files.checked_add(1).ok_or(Error::Verification)?;
            bytes = bytes.checked_add(item.bytes).ok_or(Error::Verification)?;
            if files > summary.output_files || bytes > summary.output_bytes {
                return Err(Error::Verification);
            }
            *kinds.entry(item.kind).or_default() += 1;
            append(
                &mut target,
                &Object {
                    path,
                    bytes: item.bytes,
                    sha256: item.sha256,
                },
            )
            .await?;
        }
        objects = objects
            .checked_add(resource.objects)
            .ok_or(Error::Verification)?;
        selected = selected
            .checked_add(resource.selected_objects)
            .ok_or(Error::Verification)?;
        skipped = skipped
            .checked_add(resource.skipped_objects)
            .ok_or(Error::Verification)?;
        hits += usize::from(resource.cache_hit);
    }
    if directories.len() != summary.succeeded
        || files != summary.output_files
        || bytes != summary.output_bytes
        || objects != summary.unity_objects
        || selected != summary.selected_unity_objects
        || skipped != summary.skipped_unity_objects
        || hits != summary.cache_hits
        || kinds != summary.payloads
    {
        return Err(Error::Verification);
    }
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .map_err(|_| Error::Io)?;
    while let Some(entry) = entries.next_entry().await.map_err(|_| Error::Io)? {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Verification)?;
        let kind = entry.file_type().await.map_err(|_| Error::Io)?;
        if !((kind.is_dir() && directories.contains(&name))
            || (kind.is_file() && matches!(name.as_str(), "summary.json" | "resources.jsonl")))
        {
            return Err(Error::Verification);
        }
    }
    let summary_sha256 = hex::encode(Sha256::digest(&summary_bytes));
    let journal_sha256 = hex::encode(journal_hash.finalize());
    // Detect receipt edits during verification before returning the allowlist.
    if file_hash(&summary_path, summary_bytes.len() as u64).await? != summary_sha256
        || file_hash(&journal_path, journal_bytes).await? != journal_sha256
    {
        return Err(Error::Verification);
    }
    append(
        &mut target,
        &Object {
            path: "summary.json".into(),
            bytes: summary_bytes.len() as u64,
            sha256: summary_sha256.clone(),
        },
    )
    .await?;
    append(
        &mut target,
        &Object {
            path: "resources.jsonl".into(),
            bytes: journal_bytes,
            sha256: journal_sha256.clone(),
        },
    )
    .await?;
    target.flush().await.map_err(|_| Error::Io)?;
    target.sync_all().await.map_err(|_| Error::Io)?;
    Ok(VerifiedExport {
        inventory,
        report: Report {
            schema_version: 1,
            region,
            resources_verified: directories.len(),
            files_verified: files,
            bytes_verified: bytes,
            full_catalog: summary.full_catalog,
            full_export: summary.full_export,
            summary_sha256,
            journal_sha256,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("00000")).unwrap();
        std::fs::write(root.path().join("00000/payload.bin"), b"synthetic export").unwrap();
        let report = sonic_rs::json!({
            "source":"synthetic.bundle", "source_sha256":"a".repeat(64), "output_directory":"00000",
            "objects":1, "selected_objects":1, "skipped_objects":0, "errors":[],
            "outputs":[{"path":"payload.bin", "kind":"binary", "bytes":16,
                        "sha256":hex::encode(Sha256::digest(b"synthetic export"))}]
        });
        // This synthetic payload is 16 bytes; no game data is used.
        let mut report = sonic_rs::to_vec(&report).unwrap();
        report.push(b'\n');
        std::fs::write(root.path().join("resources.jsonl"), report).unwrap();
        let summary = ExportSummary {
            schema_version: 4,
            region: Region::Jp,
            platform: "iOS".into(),
            complete: true,
            retained: true,
            input_files: 1,
            catalog_files: 1,
            succeeded: 1,
            output_files: 1,
            output_bytes: 16,
            unity_objects: 1,
            selected_unity_objects: 1,
            catalog_sha256: "b".repeat(64),
            full_catalog: true,
            full_export: true,
            payloads: BTreeMap::from([("binary".into(), 1)]),
            ..Default::default()
        };
        std::fs::write(
            root.path().join("summary.json"),
            sonic_rs::to_vec(&summary).unwrap(),
        )
        .unwrap();
        root
    }
    #[tokio::test]
    async fn complete_inventory_and_scope_are_verified() {
        let root = fixture();
        let verified = prepare(root.path(), Region::Jp).await.unwrap();
        assert_eq!(verified.report.files_verified, 1);
        assert_eq!(verified.report.bytes_verified, 16);
        let inventory = std::fs::read_to_string(verified.inventory.path()).unwrap();
        assert_eq!(inventory.lines().count(), 3);
        assert!(inventory.contains("00000/payload.bin"));
        assert!(verify(root.path(), Region::Tw).await.is_err());
        assert!(verify(root.path(), Region::Cn).await.is_err());
        let path = root.path().join("summary.json");
        let mut summary: ExportSummary =
            sonic_rs::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        summary.full_catalog = false;
        summary.full_export = false;
        summary.catalog_files = 2;
        std::fs::write(path, sonic_rs::to_vec(&summary).unwrap()).unwrap();
        let report = verify(root.path(), Region::Jp).await.unwrap();
        assert!(!report.full_catalog && !report.full_export);
    }
    #[tokio::test]
    async fn corruption_missing_extra_files_and_incomplete_receipts_are_rejected() {
        for mutation in 0..8 {
            let root = fixture();
            match mutation {
                0 => std::fs::write(root.path().join("00000/payload.bin"), b"corrupt contents")
                    .unwrap(),
                1 => std::fs::remove_file(root.path().join("00000/payload.bin")).unwrap(),
                2 => std::fs::write(root.path().join("00000/unlisted"), b"extra").unwrap(),
                3 => std::fs::write(root.path().join("unlisted"), b"extra").unwrap(),
                4 => {
                    let path = root.path().join("resources.jsonl");
                    let mut bytes = std::fs::read(&path).unwrap();
                    bytes.extend(bytes.clone());
                    std::fs::write(path, bytes).unwrap();
                }
                _ => {
                    let path = root.path().join("summary.json");
                    let mut summary: ExportSummary =
                        sonic_rs::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    match mutation {
                        5 => summary.complete = false,
                        6 => summary.retained = false,
                        _ => summary.output_bytes += 1,
                    }
                    std::fs::write(path, sonic_rs::to_vec(&summary).unwrap()).unwrap();
                }
            }
            assert!(
                verify(root.path(), Region::Jp).await.is_err(),
                "mutation {mutation}"
            );
        }
    }
    #[tokio::test]
    async fn unsafe_paths_and_oversized_reports_are_rejected() {
        for path in [
            "../outside",
            "/absolute",
            "nested/../../outside",
            "C:/outside",
            "x\\outside",
            "./payload.bin",
            "x//y",
        ] {
            let root = fixture();
            let journal = root.path().join("resources.jsonl");
            let text = std::fs::read_to_string(&journal)
                .unwrap()
                .replace("payload.bin", &path.replace('\\', "\\\\"));
            std::fs::write(journal, text).unwrap();
            assert!(verify(root.path(), Region::Jp).await.is_err());
        }
        let root = fixture();
        std::fs::write(
            root.path().join("resources.jsonl"),
            vec![b' '; MAX_REPORT as usize + 1],
        )
        .unwrap();
        assert!(verify(root.path(), Region::Jp).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn file_and_directory_links_are_rejected_without_following_them() {
        let root = fixture();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("payload.bin"), b"synthetic export").unwrap();
        std::fs::remove_file(root.path().join("00000/payload.bin")).unwrap();
        std::os::unix::fs::symlink(
            external.path().join("payload.bin"),
            root.path().join("00000/payload.bin"),
        )
        .unwrap();
        assert!(verify(root.path(), Region::Jp).await.is_err());
        std::fs::remove_dir_all(root.path().join("00000")).unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("00000")).unwrap();
        assert!(verify(root.path(), Region::Jp).await.is_err());
    }
}
