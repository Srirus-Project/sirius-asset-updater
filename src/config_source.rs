//! Remote loading of the main download configuration.
//!
//! Restores the original `HARUKI_CONFIG_URI` + `HARUKI_CONFIG_OPENDAL_*` bootstrap, adapted to
//! the OpenDAL backends Sirius already builds for publication (`fs`, `s3`). The URI names only
//! the backend and object key; every backend setting, including credential *references*, comes
//! from typed `SIRIUS_ASSET_CONFIG_SOURCE__*` variables decoded with `deny_unknown_fields`.
//! S3 access reuses the storage validation, credential-file, STS and transport code, so the same
//! verified-TLS, no-proxy, no-redirect policy applies. Errors are static and never contain the
//! URI, endpoint, key, credential names or credential values.
use crate::{
    config_env::{self, Document},
    storage, Error,
};
use opendal::{services, Operator};
use serde::Deserialize;
use std::{
    path::{Component, PathBuf},
    time::Duration,
};

/// Remote location of the download configuration (`opendal://fs/KEY` or `opendal://s3/KEY`).
pub const URI_ENV: &str = "SIRIUS_ASSET_CONFIG_URI";
/// Local path of the download configuration; mutually exclusive with [`URI_ENV`].
pub const PATH_ENV: &str = "SIRIUS_ASSET_CONFIG_PATH";
/// Local file used when neither variable is set.
pub const DEFAULT_PATH: &str = "sirius-asset-config.yaml";
/// Largest remote configuration object accepted.
pub const MAX_BYTES: u64 = 1024 * 1024;
const URI_PREFIX: &str = "opendal://";
const MAX_URI_BYTES: usize = 2048;
const MAX_KEY_BYTES: usize = 1024;

/// Typed bootstrap decoded from `SIRIUS_ASSET_CONFIG_SOURCE__*`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    #[serde(default = "timeout")]
    timeout_seconds: u64,
    #[serde(default)]
    fs: Option<FsSource>,
    #[serde(default)]
    s3: Option<S3Source>,
}
fn timeout() -> u64 {
    30
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FsSource {
    root: PathBuf,
}
/// Read-only subset of the publication S3 backend; write/ACL options do not apply.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct S3Source {
    endpoint: String,
    bucket: String,
    region: String,
    #[serde(default = "path_style")]
    path_style: bool,
    #[serde(default)]
    request_payer: bool,
    #[serde(default)]
    credentials_file: Option<Box<crate::storage_credentials::Config>>,
    #[serde(default)]
    assume_role: Option<Box<crate::storage_sts::Config>>,
    #[serde(default)]
    access_key_id_env: String,
    #[serde(default)]
    secret_access_key_env: String,
    #[serde(default)]
    session_token_env: Option<String>,
}
fn path_style() -> bool {
    true
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Fs,
    S3,
}

/// Where the download configuration comes from, decided only by the environment.
pub(crate) enum Location {
    Local(PathBuf),
    Remote(Box<Remote>),
}
/// A validated remote source. Deliberately not `Debug`: it carries the object key.
pub(crate) struct Remote {
    key: String,
    source: Source,
    scheme: Scheme,
}

/// Resolve the download configuration location from a variable snapshot.
///
/// `SIRIUS_ASSET_CONFIG_URI` (non-empty) selects a remote source and must not be combined with
/// `SIRIUS_ASSET_CONFIG_PATH`; bootstrap variables without a URI are also an error, so a
/// half-configured remote source never silently falls back to a local file.
pub(crate) fn locate(vars: &[(String, String)]) -> Result<Location, Error> {
    let get = |name: &str| {
        vars.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let uri = get(URI_ENV).map(str::trim).filter(|v| !v.is_empty());
    let bootstrap = vars
        .iter()
        .any(|(k, _)| k.starts_with(Document::ConfigSource.prefix()));
    let Some(uri) = uri else {
        if bootstrap {
            return Err(Error::Config);
        }
        return Ok(Location::Local(PathBuf::from(
            get(PATH_ENV).unwrap_or(DEFAULT_PATH),
        )));
    };
    if get(PATH_ENV).is_some() {
        return Err(Error::Config);
    }
    let (scheme, key) = parse_uri(uri)?;
    let mut root = yaml_serde::Value::Mapping(yaml_serde::Mapping::new());
    config_env::apply(&mut root, Document::ConfigSource, vars)?;
    let source: Source = yaml_serde::from_value(root).map_err(|_| Error::Config)?;
    if !(1..=300).contains(&source.timeout_seconds) {
        return Err(Error::Config);
    }
    match (scheme, &source.fs, &source.s3) {
        (Scheme::Fs, Some(fs), None) => {
            let root = &fs.root;
            if !root.is_absolute()
                || root.to_str().is_none()
                || root.components().any(|c| matches!(c, Component::ParentDir))
            {
                return Err(Error::Config);
            }
        }
        (Scheme::S3, None, Some(s3)) => storage::validate_backend(&s3.backend())?,
        _ => return Err(Error::Config),
    }
    Ok(Location::Remote(Box::new(Remote {
        key,
        source,
        scheme,
    })))
}

/// Parse `opendal://<fs|s3>/<key>`. Userinfo, query, fragment, percent-encoding, backslashes,
/// whitespace, non-ASCII, empty, `.` and `..` segments are rejected rather than normalized.
fn parse_uri(uri: &str) -> Result<(Scheme, String), Error> {
    if uri.len() > MAX_URI_BYTES
        || !uri.bytes().all(|b| (0x21..=0x7e).contains(&b))
        || uri.contains(['?', '#', '%', '\\', '@'])
    {
        return Err(Error::Config);
    }
    let rest = uri.strip_prefix(URI_PREFIX).ok_or(Error::Config)?;
    let (scheme, key) = rest.split_once('/').ok_or(Error::Config)?;
    let scheme = match scheme {
        "fs" => Scheme::Fs,
        "s3" => Scheme::S3,
        _ => return Err(Error::Config),
    };
    if key.len() > MAX_KEY_BYTES
        || key
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == ".." || s.len() > 255)
    {
        return Err(Error::Config);
    }
    Ok((scheme, key.to_owned()))
}

impl S3Source {
    fn backend(&self) -> storage::Backend {
        storage::Backend::S3 {
            credentials_file: self.credentials_file.clone(),
            assume_role: self.assume_role.clone(),
            request_payer: self.request_payer,
            endpoint: self.endpoint.clone(),
            write_options: Box::default(),
            path_style: self.path_style,
            public_read: false,
            public_read_include: vec![],
            public_read_exclude: vec![],
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            access_key_id_env: self.access_key_id_env.clone(),
            secret_access_key_env: self.secret_access_key_env.clone(),
            session_token_env: self.session_token_env.clone(),
        }
    }
}

impl Remote {
    fn operator(&self) -> Result<Operator, Error> {
        match (self.scheme, &self.source.fs, &self.source.s3) {
            (Scheme::Fs, Some(fs), _) => {
                Operator::new(services::Fs::default().root(fs.root.to_str().ok_or(Error::Config)?))
                    .map_err(|_| Error::Config)
            }
            (Scheme::S3, _, Some(s3)) => storage::s3_operator(&s3.backend(), false),
            _ => Err(Error::Config),
        }
    }
    /// Fetch one bounded snapshot of the object: stat, reject non-files and oversize objects,
    /// then read exactly the stated length so a concurrently growing object cannot exceed the cap.
    pub(crate) async fn fetch(&self) -> Result<String, Error> {
        let limit = Duration::from_secs(self.source.timeout_seconds);
        let bytes = tokio::time::timeout(limit, async {
            let op = self.operator()?;
            let meta = op.stat(&self.key).await.map_err(source_error)?;
            if !meta.mode().is_file() {
                return Err(Error::RemoteConfig);
            }
            let length = meta.content_length();
            if length > MAX_BYTES {
                return Err(Error::Size);
            }
            if length == 0 {
                return Err(Error::Config);
            }
            let data = op
                .read_with(&self.key)
                .range(0..length)
                .await
                .map_err(source_error)?;
            if data.len() as u64 != length {
                return Err(Error::RemoteConfig);
            }
            Ok(data.to_vec())
        })
        .await
        .map_err(|_| Error::Transport)??;
        String::from_utf8(bytes).map_err(|_| Error::Config)
    }
}
fn source_error(error: opendal::Error) -> Error {
    if error.is_temporary() {
        Error::Transport
    } else {
        Error::RemoteConfig
    }
}

/// Read the download configuration text from the location this process's environment selects.
/// Local files keep their existing semantics; remote objects are bounded and time-limited.
pub async fn read_download_config() -> Result<String, Error> {
    read_with(&config_env::process_vars()).await
}
pub(crate) async fn read_with(vars: &[(String, String)]) -> Result<String, Error> {
    match locate(vars)? {
        Location::Local(path) => std::fs::read_to_string(path).map_err(|_| Error::Config),
        Location::Remote(remote) => remote.fetch().await,
    }
}
/// Read and decode the download configuration, applying `SIRIUS_ASSET__` overrides after the
/// fetch exactly as for a local file (the binary does the same via `config_env::from_str`).
#[cfg(test)]
pub(crate) async fn load_with<T: serde::de::DeserializeOwned>(
    vars: &[(String, String)],
) -> Result<T, Error> {
    config_env::from_str_with(&read_with(vars).await?, Document::Download, vars)
}

#[cfg(test)]
#[path = "config_source_tests.rs"]
mod tests;
