//! Verified, region-scoped publication of retained Sirius exports.
use crate::{
    export_verify::{self, Object},
    region::Region,
    Error,
};
use futures_util::{stream, StreamExt};
use opendal::{services, HttpTransporter, OperationContext, Operator};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::watch,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub providers: Vec<Provider>,
    #[serde(default = "concurrency")]
    pub concurrency: usize,
    #[serde(skip)]
    pub(crate) service_upload_gate: Option<Arc<tokio::sync::Semaphore>>,
    #[serde(default = "attempts")]
    pub attempts: usize,
    #[serde(default = "timeout")]
    pub object_timeout_seconds: u64,
    #[serde(default = "delay")]
    pub retry_delay_ms: u64,
    #[serde(default)]
    pub remove_local_after_upload: bool,
}
fn concurrency() -> usize {
    4
}
fn attempts() -> usize {
    3
}
fn timeout() -> u64 {
    300
}
fn delay() -> u64 {
    500
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub name: String,
    #[serde(default = "prefix")]
    pub prefix: String,
    #[serde(default)]
    pub public_base_url: Option<String>,
    pub backend: Backend,
}
fn path_style() -> bool {
    true
}
fn prefix() -> String {
    "assets".into()
}
#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Backend {
    Local {
        directory: PathBuf,
    },
    S3 {
        #[serde(default)]
        credentials_file: Option<Box<crate::storage_credentials::Config>>,
        #[serde(default)]
        assume_role: Option<Box<crate::storage_sts::Config>>,
        #[serde(default)]
        request_payer: bool,
        endpoint: String,
        #[serde(default)]
        write_options: Box<S3WriteOptions>,
        #[serde(default = "path_style")]
        path_style: bool,
        #[serde(default)]
        public_read: bool,
        #[serde(default)]
        public_read_include: Vec<String>,
        #[serde(default)]
        public_read_exclude: Vec<String>,
        bucket: String,
        region: String,
        #[serde(default)]
        access_key_id_env: String,
        #[serde(default)]
        secret_access_key_env: String,
        #[serde(default)]
        session_token_env: Option<String>,
    },
}
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3WriteOptions {
    pub default_acl: Option<String>,
    pub customer_key_base64_env: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub storage_class: Option<String>,
    pub server_side_encryption: Option<String>,
    pub kms_key_id_env: Option<String>,
}
impl S3WriteOptions {
    fn validate(&self) -> Result<(), Error> {
        if self.default_acl.as_deref().is_some_and(|v| {
            !matches!(
                v,
                "private"
                    | "public-read"
                    | "public-read-write"
                    | "authenticated-read"
                    | "aws-exec-read"
                    | "bucket-owner-read"
                    | "bucket-owner-full-control"
            )
        }) {
            return Err(Error::Config);
        }
        self.customer_key()?;
        if self
            .checksum_algorithm
            .as_deref()
            .is_some_and(|s| s != "crc32c")
        {
            return Err(Error::Config);
        }
        if self.storage_class.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 64
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        }) || self
            .server_side_encryption
            .as_deref()
            .is_some_and(|value| !matches!(value, "AES256" | "aws:kms"))
            || self.kms_key_id_env.is_some()
                && self.server_side_encryption.as_deref() != Some("aws:kms")
        {
            return Err(Error::Config);
        }
        if let Some(name) = &self.kms_key_id_env {
            let value = secret(name)?;
            if value.len() > 2048 || !value.bytes().all(|b| (33..=126).contains(&b)) {
                return Err(Error::Config);
            }
        }
        Ok(())
    }
    fn public_acl(&self) -> Option<&str> {
        self.default_acl
            .as_deref()
            .filter(|v| matches!(*v, "public-read" | "public-read-write"))
    }
    fn customer_key(&self) -> Result<Option<[u8; 32]>, Error> {
        use base64::Engine;
        let Some(name) = &self.customer_key_base64_env else {
            return Ok(None);
        };
        if self.server_side_encryption.is_some() || self.kms_key_id_env.is_some() {
            return Err(Error::Config);
        }
        let value = secret(name)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|_| Error::Config)?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| Error::Config)?;
        Ok(Some(key))
    }
    fn apply(&self, mut builder: services::S3) -> Result<services::S3, Error> {
        self.validate()?;
        if let Some(key) = self.customer_key()? {
            builder = builder.server_side_encryption_with_customer_key("AES256", &key);
        }
        if let Some(value) = &self.checksum_algorithm {
            builder = builder.checksum_algorithm(value);
        }
        if let Some(value) = &self.storage_class {
            builder = builder.default_storage_class(value);
        }
        if let Some(value) = &self.server_side_encryption {
            builder = builder.server_side_encryption(value);
        }
        if let Some(name) = &self.kms_key_id_env {
            builder = builder.server_side_encryption_aws_kms_key_id(&secret(name)?);
        }
        Ok(builder)
    }
}
struct PublicReadPolicy {
    all: bool,
    include: Vec<regex::Regex>,
    exclude: Vec<regex::Regex>,
}
impl PublicReadPolicy {
    fn matches(&self, path: &str) -> bool {
        (self.all || self.include.iter().any(|r| r.is_match(path)))
            && !self.exclude.iter().any(|r| r.is_match(path))
    }
}
fn acl_rules(patterns: &[String]) -> Result<Vec<regex::Regex>, Error> {
    if patterns.len() > 128 {
        return Err(Error::Config);
    }
    patterns
        .iter()
        .map(|pattern| {
            if pattern.trim().is_empty() || pattern.len() > 4096 {
                return Err(Error::Config);
            }
            regex::RegexBuilder::new(pattern)
                .size_limit(1024 * 1024)
                .build()
                .map_err(|_| Error::Config)
        })
        .collect()
}
#[derive(Clone, Default)]
pub struct UploadProgress {
    pub phase: String,
    pub completed: u64,
    pub total: u64,
    pub bytes: u64,
}
impl UploadProgress {
    fn emit(&self, channel: &Option<watch::Sender<Self>>) {
        if let Some(channel) = channel {
            channel.send_replace(self.clone());
        }
    }
}
#[derive(Serialize)]
pub struct Publication {
    pub schema_version: u8,
    pub id: String,
    pub region: Region,
    pub files: usize,
    pub bytes: u64,
    pub providers: Vec<Target>,
    pub local_removed: bool,
}
#[derive(Serialize)]
pub struct Target {
    pub name: String,
    pub prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}
#[derive(Serialize)]
pub struct Plan {
    pub preview: bool,
    pub region: Region,
    pub example_publication_id: String,
    pub providers: Vec<Target>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    pub input: PathBuf,
    pub region: Region,
    pub storage: Config,
}
fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub(crate) fn secret(name: &str) -> Result<String, Error> {
    if name.is_empty()
        || name.len() > 256
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(Error::Config);
    }
    let value = std::env::var(name).map_err(|_| Error::Secret)?;
    if value.trim().is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
        return Err(Error::Secret);
    }
    Ok(value)
}
fn storage_error(error: opendal::Error) -> Error {
    if error.is_temporary() {
        Error::Transport
    } else {
        Error::Storage
    }
}
impl Config {
    /// Validate every operational Sirius region without performing storage I/O.
    pub fn validate(&self) -> Result<(), Error> {
        for region in [Region::Jp, Region::Tw, Region::En, Region::Kr] {
            self.resolved(region)?.validate_resolved()?;
        }
        Ok(())
    }
    fn resolved(&self, region: Region) -> Result<Self, Error> {
        if region == Region::Cn {
            return Err(Error::ReservedRegion);
        }
        let mut resolved = self.clone();
        resolved.providers = self
            .providers
            .iter()
            .map(|p| p.resolved(region))
            .collect::<Result<_, _>>()?;
        Ok(resolved)
    }
    fn validate_resolved(&self) -> Result<(), Error> {
        if self.providers.is_empty()
            || self.providers.len() > 16
            || !(1..=32).contains(&self.concurrency)
            || !(1..=8).contains(&self.attempts)
            || !(1..=3600).contains(&self.object_timeout_seconds)
            || !(1..=10_000).contains(&self.retry_delay_ms)
        {
            return Err(Error::Config);
        }
        let mut names = HashSet::new();
        for p in &self.providers {
            p.public_read_policy()?;
            p.public_base()?;
            if !component(&p.name)
                || !names.insert(&p.name)
                || p.prefix.len() > 512
                || !p.prefix.split('/').all(component)
            {
                return Err(Error::Config);
            }
            match &p.backend {
                Backend::Local { directory } => {
                    if directory.as_os_str().is_empty() {
                        return Err(Error::Config);
                    }
                }
                Backend::S3 {
                    credentials_file,
                    assume_role,
                    endpoint,
                    write_options,
                    path_style,
                    bucket,
                    region,
                    access_key_id_env,
                    secret_access_key_env,
                    session_token_env,
                    ..
                } => {
                    write_options.validate()?;
                    if let Some(role) = assume_role {
                        role.validate()?;
                    }
                    let url = reqwest::Url::parse(endpoint).map_err(|_| Error::Config)?;
                    let loopback = url.host_str().is_some_and(|h| {
                        h.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                    });
                    if endpoint.len() > 2048
                        || endpoint.chars().any(char::is_whitespace)
                        || endpoint.contains('\\')
                        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
                        || url.host_str().is_none()
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.query().is_some()
                        || url.fragment().is_some()
                        || url.path() != "/"
                        || (!path_style
                            && (bucket.contains('.')
                                || url.host_str().is_some_and(|host| {
                                    host.trim_matches(['[', ']'])
                                        .parse::<std::net::IpAddr>()
                                        .is_ok()
                                })))
                        || bucket.is_empty()
                        || bucket.len() > 63
                        || !bucket.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                        })
                        || !component(region)
                    {
                        return Err(Error::Config);
                    }
                    if let Some(file) = credentials_file {
                        if !access_key_id_env.is_empty()
                            || !secret_access_key_env.is_empty()
                            || session_token_env.is_some()
                        {
                            return Err(Error::Config);
                        }
                        file.read()?;
                    } else {
                        secret(access_key_id_env)?;
                        secret(secret_access_key_env)?;
                        if let Some(env) = session_token_env {
                            secret(env)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub fn plan(&self, region: Region) -> Result<Plan, Error> {
        if region == Region::Cn {
            return Err(Error::ReservedRegion);
        }
        let resolved = self.resolved(region)?;
        resolved.validate_resolved()?;
        let id = uuid::Uuid::new_v4().to_string();
        let providers = resolved
            .providers
            .iter()
            .map(|p| p.target(region, &id))
            .collect::<Result<_, _>>()?;
        Ok(Plan {
            preview: true,
            region,
            example_publication_id: id,
            providers,
        })
    }
    pub async fn run(&self, input: &Path, region: Region) -> Result<Publication, Error> {
        let (tx, rx) = watch::channel(false);
        let work = self.publish(input, region, rx);
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => result,
            _ = tokio::signal::ctrl_c() => { let _ = tx.send(true); work.await }
        }
    }
    pub async fn publish(
        &self,
        input: &Path,
        region: Region,
        stop: watch::Receiver<bool>,
    ) -> Result<Publication, Error> {
        self.publish_with_progress(input, region, stop, None).await
    }
    pub async fn publish_with_progress(
        &self,
        input: &Path,
        region: Region,
        mut stop: watch::Receiver<bool>,
        progress_channel: Option<watch::Sender<UploadProgress>>,
    ) -> Result<Publication, Error> {
        if region == Region::Cn {
            return Err(Error::ReservedRegion);
        }
        let resolved = self.resolved(region)?;
        resolved.validate_resolved()?;
        let verified = tokio::select! {
            result = export_verify::prepare(input, region) => result?,
            _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
        };
        let input = tokio::fs::canonicalize(input)
            .await
            .map_err(|_| Error::Io)?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut operators = Vec::new();
        for provider in &resolved.providers {
            let op = provider.operator(&input)?;
            let policy = provider.public_read_policy()?;
            let public_op = if policy.all || !policy.include.is_empty() {
                Some(provider.operator_with_acl(&input, true)?)
            } else {
                None
            };
            let target = provider.target(region, &id)?;
            operators.push((op, public_op, policy, target));
        }
        let mut total_bytes = 0_u64;
        let mut total_files = 0_usize;
        let mut progress = UploadProgress {
            phase: "publish".into(),
            total: (verified.report.files_verified as u64)
                .checked_add(2)
                .and_then(|n| n.checked_mul(operators.len() as u64))
                .ok_or(Error::Size)?,
            ..UploadProgress::default()
        };
        for (index, (op, public_op, policy, target)) in operators.iter().enumerate() {
            progress.phase = format!("publish_upload_{}_of_{}", index + 1, operators.len());
            progress.emit(&progress_channel);
            let inventory = tokio::fs::File::open(verified.inventory.path())
                .await
                .map_err(|_| Error::Io)?;
            let lines = BufReader::new(inventory).lines();
            let source = stream::unfold((lines, false), |(mut lines, done)| async move {
                if done {
                    return None;
                }
                match lines.next_line().await {
                    Ok(Some(line)) => Some((
                        sonic_rs::from_str::<Object>(&line).map_err(|_| Error::Verification),
                        (lines, false),
                    )),
                    Ok(None) => None,
                    Err(_) => Some((Err(Error::Io), (lines, true))),
                }
            });
            let failed = Arc::new(AtomicBool::new(false));
            let work = source
                .map(|object| {
                    let failed = failed.clone();
                    let stop = stop.clone();
                    let input = &input;
                    async move {
                        if failed.load(Ordering::Acquire) {
                            return Err(Error::Cancelled);
                        }
                        let result = async {
                            let object = object?;
                            let op = if policy.matches(&object.path) {
                                public_op.as_ref().unwrap_or(op)
                            } else {
                                op
                            };
                            self.upload(op, &target.prefix, input, &object, stop)
                                .await?;
                            Ok(object.bytes)
                        }
                        .await;
                        if result.is_err() {
                            failed.store(true, Ordering::Release);
                        }
                        result
                    }
                })
                .buffer_unordered(self.concurrency);
            tokio::pin!(work);
            let mut error = None;
            let mut bytes = 0_u64;
            let mut files = 0_usize;
            // Drain in-flight uploads, including their multipart aborts, before returning.
            while let Some(result) = work.next().await {
                match result {
                    Ok(size) => {
                        files += 1;
                        bytes = bytes.checked_add(size).ok_or(Error::Size)?;
                        progress.completed =
                            progress.completed.checked_add(1).ok_or(Error::Size)?;
                        progress.bytes = progress.bytes.checked_add(size).ok_or(Error::Size)?;
                        progress.emit(&progress_channel);
                    }
                    Err(e) => {
                        if error.is_none() {
                            error = Some(e);
                        }
                    }
                }
            }
            if let Some(error) = error {
                return Err(error);
            }
            total_bytes = bytes;
            total_files = files;
        }
        progress.phase = "publish_markers".into();
        progress.emit(&progress_channel);
        // Consumers must ignore prefixes without this last, verified completion marker.
        let marker = sonic_rs::to_vec(&verified.report).map_err(|_| Error::Verification)?;
        for (op, public_op, policy, target) in &operators {
            let op = if policy.matches("complete.json") {
                public_op.as_ref().unwrap_or(op)
            } else {
                op
            };
            let key = format!("{}/complete.json", target.prefix);
            let expected = hex::encode(Sha256::digest(&marker));
            for attempt in 0..self.attempts {
                let task = async {
                    let _permit = self.acquire_upload().await?;
                    op.write(&key, marker.clone())
                        .await
                        .map_err(storage_error)?;
                    check_remote(op, &key, marker.len() as u64, &expected).await
                };
                match controlled(
                    task,
                    &mut stop,
                    tokio::time::Instant::now() + Duration::from_secs(self.object_timeout_seconds),
                )
                .await
                {
                    Err(Error::Transport) if attempt + 1 < self.attempts => {
                        let delay = (self.retry_delay_ms * (1 << attempt)).min(30_000);
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
                            _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
                        }
                    }
                    result => {
                        result?;
                        break;
                    }
                }
            }
        }
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
        if self.remove_local_after_upload {
            progress.phase = "publish_cleanup".into();
            progress.emit(&progress_channel);
            // Refuse to delete a changed tree or unlisted local additions.
            let checked = tokio::select! {
                result = export_verify::verify(&input, region) => result?,
                _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
            };
            if checked.summary_sha256 != verified.report.summary_sha256
                || checked.journal_sha256 != verified.report.journal_sha256
            {
                return Err(Error::Verification);
            }
            // All configured destinations already have verified objects and completion markers.
            tokio::fs::remove_dir_all(&input)
                .await
                .map_err(|_| Error::Io)?;
        }
        progress.phase = "publish".into();
        progress.emit(&progress_channel);
        Ok(Publication {
            schema_version: 1,
            id,
            region,
            files: total_files,
            bytes: total_bytes,
            providers: operators
                .into_iter()
                .map(|(_, _, _, target)| target)
                .collect(),
            local_removed: self.remove_local_after_upload,
        })
    }
    async fn upload(
        &self,
        op: &Operator,
        prefix: &str,
        input: &Path,
        object: &Object,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), Error> {
        for attempt in 0..self.attempts {
            match self.upload_once(op, prefix, input, object, &mut stop).await {
                Err(Error::Transport) if attempt + 1 < self.attempts => {
                    let delay = (self.retry_delay_ms * (1 << attempt)).min(30_000);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
                        _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
                    }
                }
                result => return result,
            }
        }
        Err(Error::Storage)
    }
    async fn acquire_upload(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, Error> {
        match &self.service_upload_gate {
            Some(gate) => gate
                .clone()
                .acquire_owned()
                .await
                .map(Some)
                .map_err(|_| Error::Cancelled),
            None => Ok(None),
        }
    }
    async fn upload_once(
        &self,
        op: &Operator,
        prefix: &str,
        input: &Path,
        object: &Object,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), Error> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.object_timeout_seconds);
        let _permit = controlled(self.acquire_upload(), stop, deadline).await?;
        let key = format!("{prefix}/{}", object.path);
        let source = input.join(&object.path);
        let meta = controlled(
            async {
                tokio::fs::symlink_metadata(&source)
                    .await
                    .map_err(|_| Error::Io)
            },
            stop,
            deadline,
        )
        .await?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != object.bytes {
            return Err(Error::Verification);
        }
        let mut writer = controlled(
            async {
                op.writer_with(&key)
                    .chunk(8 * 1024 * 1024)
                    .concurrent(1)
                    .await
                    .map_err(storage_error)
            },
            stop,
            deadline,
        )
        .await?;
        let operation = async {
            let mut file = tokio::fs::File::open(source).await.map_err(|_| Error::Io)?;
            let mut buffer = vec![0; 64 * 1024];
            let mut hasher = Sha256::new();
            let mut size = 0_u64;
            loop {
                let n = file.read(&mut buffer).await.map_err(|_| Error::Io)?;
                if n == 0 {
                    break;
                }
                size = size.checked_add(n as u64).ok_or(Error::Verification)?;
                if size > object.bytes {
                    return Err(Error::Verification);
                }
                hasher.update(&buffer[..n]);
                writer
                    .write(buffer[..n].to_vec())
                    .await
                    .map_err(storage_error)?;
            }
            if size != object.bytes || hex::encode(hasher.finalize()) != object.sha256 {
                return Err(Error::Verification);
            }
            writer.close().await.map_err(storage_error)?;
            check_remote(op, &key, object.bytes, &object.sha256).await
        };
        let result = controlled(operation, stop, deadline).await;
        if result.is_err() {
            // Best effort, bounded abort; interrupted remote uploads may need bucket lifecycle cleanup.
            let _ = tokio::time::timeout(Duration::from_secs(3), writer.abort()).await;
        }
        result
    }
}
async fn controlled<T>(
    task: impl std::future::Future<Output = Result<T, Error>>,
    stop: &mut watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> Result<T, Error> {
    if *stop.borrow() {
        return Err(Error::Cancelled);
    }
    tokio::select! {
        result = tokio::time::timeout_at(deadline, task) => result.map_err(|_| Error::Transport)?,
        _ = crate::service::cancelled(stop) => Err(Error::Cancelled),
    }
}
async fn check_remote(op: &Operator, key: &str, bytes: u64, hash: &str) -> Result<(), Error> {
    if op.stat(key).await.map_err(storage_error)?.content_length() != bytes {
        return Err(Error::Verification);
    }
    let reader = op
        .reader_with(key)
        .chunk(1024 * 1024)
        .concurrent(1)
        .await
        .map_err(storage_error)?;
    let mut stream = reader
        .into_bytes_stream(0..bytes)
        .await
        .map_err(storage_error)?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    while let Some(part) = stream.next().await {
        let part = part.map_err(|error| {
            if error
                .get_ref()
                .and_then(|e| e.downcast_ref::<opendal::Error>())
                .is_some_and(|e| e.is_temporary())
            {
                Error::Transport
            } else {
                Error::Storage
            }
        })?;
        size = size
            .checked_add(part.len() as u64)
            .ok_or(Error::Verification)?;
        if size > bytes {
            return Err(Error::Verification);
        }
        hasher.update(&part);
    }
    if size != bytes || hex::encode(hasher.finalize()) != hash {
        return Err(Error::Verification);
    }
    Ok(())
}
fn region_template(value: &str, region: Region) -> Result<String, Error> {
    if value.len() > 4096 {
        return Err(Error::Config);
    }
    let resolved = value
        .replace("{region}", region.name())
        .replace("{server}", region.name());
    if resolved.contains(['{', '}']) {
        return Err(Error::Config);
    }
    Ok(resolved)
}
impl Provider {
    fn resolved(&self, region: Region) -> Result<Self, Error> {
        let mut value = self.clone();
        value.prefix = region_template(&self.prefix, region)?;
        value.public_base_url = self
            .public_base_url
            .as_ref()
            .map(|s| region_template(s, region))
            .transpose()?;
        match &mut value.backend {
            Backend::Local { directory } => {
                if let Some(text) = directory.to_str() {
                    *directory = region_template(text, region)?.into();
                }
            }
            Backend::S3 {
                bucket, endpoint, ..
            } => {
                *bucket = region_template(bucket, region)?;
                *endpoint = region_template(endpoint, region)?;
            }
        }
        Ok(value)
    }
    fn public_base(&self) -> Result<Option<reqwest::Url>, Error> {
        let Some(value) = &self.public_base_url else {
            return Ok(None);
        };
        let url = reqwest::Url::parse(value).map_err(|_| Error::Config)?;
        if value.len() > 2048
            || value.chars().any(char::is_whitespace)
            || value.contains('\\')
            || url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Config);
        }
        Ok(Some(url))
    }
    fn target(&self, region: Region, id: &str) -> Result<Target, Error> {
        let prefix = format!("{}/{}/publications/{id}", self.prefix, region.name());
        let public_url = self.public_base()?.map(|mut url| {
            url.path_segments_mut()
                .expect("validated HTTPS URL")
                .pop_if_empty()
                .extend(prefix.split('/'))
                .push("");
            url.to_string()
        });
        Ok(Target {
            name: self.name.clone(),
            prefix,
            public_url,
        })
    }

    fn public_read_policy(&self) -> Result<PublicReadPolicy, Error> {
        match &self.backend {
            Backend::S3 {
                write_options,
                public_read,
                public_read_include,
                public_read_exclude,
                ..
            } => Ok(PublicReadPolicy {
                all: *public_read || write_options.public_acl().is_some(),
                include: acl_rules(public_read_include)?,
                exclude: acl_rules(public_read_exclude)?,
            }),
            Backend::Local { .. } => Ok(PublicReadPolicy {
                all: false,
                include: vec![],
                exclude: vec![],
            }),
        }
    }
    fn operator(&self, source: &Path) -> Result<Operator, Error> {
        self.operator_with_acl(source, false)
    }
    fn operator_with_acl(&self, source: &Path, public: bool) -> Result<Operator, Error> {
        match &self.backend {
            Backend::Local { directory } => {
                let source = std::fs::canonicalize(source).map_err(|_| Error::Io)?;
                let absolute = if directory.is_absolute() {
                    directory.clone()
                } else {
                    std::env::current_dir()
                        .map_err(|_| Error::Io)?
                        .join(directory)
                };
                if absolute
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(Error::Config);
                }
                let mut ancestor = absolute.as_path();
                while !ancestor.exists() {
                    ancestor = ancestor.parent().ok_or(Error::Config)?;
                }
                if std::fs::canonicalize(ancestor)
                    .map_err(|_| Error::Io)?
                    .starts_with(&source)
                {
                    return Err(Error::Config);
                }
                std::fs::create_dir_all(directory).map_err(|_| Error::Io)?;
                let root = std::fs::canonicalize(directory).map_err(|_| Error::Io)?;
                if root.starts_with(&source) || source.starts_with(&root) {
                    return Err(Error::Config);
                }
                let root = root.to_str().ok_or(Error::Config)?;
                Operator::new(
                    services::Fs::default()
                        .root(root)
                        .atomic_write_dir(&format!("{root}/.staging")),
                )
                .map_err(storage_error)
            }
            Backend::S3 {
                credentials_file,
                assume_role,
                request_payer,
                endpoint,
                write_options,
                path_style,
                bucket,
                region,
                access_key_id_env,
                secret_access_key_env,
                session_token_env,
                ..
            } => {
                let mut builder = services::S3::default()
                    .endpoint(endpoint)
                    .bucket(bucket)
                    .region(region)
                    .disable_config_load()
                    .disable_ec2_metadata();
                if *request_payer {
                    builder = builder.enable_request_payer();
                }
                builder = write_options.apply(builder)?;
                if public {
                    builder =
                        builder.default_acl(write_options.public_acl().unwrap_or("public-read"));
                } else if let Some(acl) = &write_options.default_acl {
                    // A public default participates in selection; exclusions use explicit private ACL.
                    builder = builder.default_acl(if write_options.public_acl().is_some() {
                        "private"
                    } else {
                        acl
                    });
                }
                if !path_style {
                    builder = builder.enable_virtual_host_style();
                }
                if let Some(file) = credentials_file {
                    let chain = match assume_role {
                        Some(role) => role.chain_from_source((**file).clone())?,
                        None => reqsign_core::ProvideCredentialChain::new().push((**file).clone()),
                    };
                    builder = builder.credential_provider_chain(chain);
                } else {
                    builder = builder
                        .access_key_id(&secret(access_key_id_env)?)
                        .secret_access_key(&secret(secret_access_key_env)?);
                    let token = session_token_env
                        .as_ref()
                        .map(|name| secret(name))
                        .transpose()?;
                    if let Some(value) = &token {
                        builder = builder.session_token(value);
                    }
                    if let Some(role) = assume_role {
                        builder = builder.credential_provider_chain(role.chain(
                            &secret(access_key_id_env)?,
                            &secret(secret_access_key_env)?,
                            token.as_deref(),
                        )?);
                    }
                }
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy()
                    .no_gzip()
                    .no_brotli()
                    .no_deflate()
                    .no_zstd()
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|_| Error::Config)?;
                let transport = HttpTransporter::new(
                    opendal_http_transport_reqwest::ReqwestTransport::new(client),
                );
                Ok(Operator::new(builder)
                    .map_err(storage_error)?
                    .with_context(OperationContext::new().with_http_transport(transport)))
            }
        }
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
