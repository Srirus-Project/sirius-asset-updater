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
        endpoint: String,
        #[serde(default = "path_style")]
        path_style: bool,
        bucket: String,
        region: String,
        access_key_id_env: String,
        secret_access_key_env: String,
        #[serde(default)]
        session_token_env: Option<String>,
    },
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
fn secret(name: &str) -> Result<String, Error> {
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
    pub fn validate(&self) -> Result<(), Error> {
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
                    endpoint,
                    path_style,
                    bucket,
                    region,
                    access_key_id_env,
                    secret_access_key_env,
                    session_token_env,
                } => {
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
                    secret(access_key_id_env)?;
                    secret(secret_access_key_env)?;
                    if let Some(env) = session_token_env {
                        secret(env)?;
                    }
                }
            }
        }
        Ok(())
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
        mut stop: watch::Receiver<bool>,
    ) -> Result<Publication, Error> {
        if region == Region::Cn {
            return Err(Error::ReservedRegion);
        }
        self.validate()?;
        let verified = tokio::select! {
            result = export_verify::prepare(input, region) => result?,
            _ = crate::service::cancelled(&mut stop) => return Err(Error::Cancelled),
        };
        let input = tokio::fs::canonicalize(input)
            .await
            .map_err(|_| Error::Io)?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut operators = Vec::new();
        for provider in &self.providers {
            let op = provider.operator(&input)?;
            let prefix = format!("{}/{}/publications/{id}", provider.prefix, region.name());
            operators.push((
                op,
                Target {
                    name: provider.name.clone(),
                    prefix,
                },
            ));
        }
        let mut total_bytes = 0_u64;
        let mut total_files = 0_usize;
        for (op, target) in &operators {
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
        // Consumers must ignore prefixes without this last, verified completion marker.
        let marker = sonic_rs::to_vec(&verified.report).map_err(|_| Error::Verification)?;
        for (op, target) in &operators {
            let key = format!("{}/complete.json", target.prefix);
            let expected = hex::encode(Sha256::digest(&marker));
            for attempt in 0..self.attempts {
                let task = async {
                    op.write(&key, marker.clone())
                        .await
                        .map_err(storage_error)?;
                    check_remote(op, &key, marker.len() as u64, &expected).await
                };
                match controlled(task, &mut stop, self.object_timeout_seconds).await {
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
        Ok(Publication {
            schema_version: 1,
            id,
            region,
            files: total_files,
            bytes: total_bytes,
            providers: operators.into_iter().map(|(_, target)| target).collect(),
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
    async fn upload_once(
        &self,
        op: &Operator,
        prefix: &str,
        input: &Path,
        object: &Object,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), Error> {
        let key = format!("{prefix}/{}", object.path);
        let source = input.join(&object.path);
        let meta = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|_| Error::Io)?;
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
            self.object_timeout_seconds,
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
        let result = controlled(operation, stop, self.object_timeout_seconds).await;
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
    seconds: u64,
) -> Result<T, Error> {
    if *stop.borrow() {
        return Err(Error::Cancelled);
    }
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(seconds), task) => result.map_err(|_| Error::Transport)?,
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
impl Provider {
    fn operator(&self, source: &Path) -> Result<Operator, Error> {
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
                endpoint,
                path_style,
                bucket,
                region,
                access_key_id_env,
                secret_access_key_env,
                session_token_env,
            } => {
                let mut builder = services::S3::default()
                    .endpoint(endpoint)
                    .bucket(bucket)
                    .region(region)
                    .access_key_id(&secret(access_key_id_env)?)
                    .secret_access_key(&secret(secret_access_key_env)?)
                    .disable_config_load()
                    .disable_ec2_metadata();
                if !path_style {
                    builder = builder.enable_virtual_host_style();
                }
                if let Some(name) = session_token_env {
                    builder = builder.session_token(&secret(name)?);
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
