//! Explicit, bounded shared-credential files. No ambient profile or executable providers.
use crate::Error;
use reqsign_aws_v4::Credential;
use reqsign_core::{time::Timestamp, Context, ProvideCredential};
use serde::Deserialize;
use std::{collections::BTreeMap, fmt, io::Read, path::PathBuf, time::Duration};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub path: PathBuf,
    #[serde(default = "profile")]
    pub profile: String,
    #[serde(default = "refresh")]
    pub refresh_seconds: u64,
}
fn profile() -> String {
    "default".into()
}
fn refresh() -> u64 {
    60
}
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SiriusCredentialFile")
    }
}
impl Config {
    pub(crate) fn read(&self) -> Result<Credential, Error> {
        if self.path.as_os_str().is_empty()
            || self.profile.is_empty()
            || self.profile.len() > 128
            || self
                .profile
                .chars()
                .any(|c| c.is_control() || matches!(c, '[' | ']'))
            || !(1..=3600).contains(&self.refresh_seconds)
        {
            return Err(Error::Config);
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        if std::fs::symlink_metadata(&self.path)
            .map_err(|_| Error::Secret)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Secret);
        }
        let file = options.open(&self.path).map_err(|_| Error::Secret)?;
        let meta = file.metadata().map_err(|_| Error::Secret)?;
        if !meta.is_file() || meta.len() > 65536 {
            return Err(Error::Secret);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(Error::Secret);
            }
        }
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Secret)?;
        if bytes.len() > 65536 {
            return Err(Error::Secret);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Secret)?;
        let values = parse(text, &self.profile)?;
        let access = values.get("aws_access_key_id").ok_or(Error::Secret)?;
        let secret = values.get("aws_secret_access_key").ok_or(Error::Secret)?;
        let token = values.get("aws_session_token").cloned();
        for value in [Some(access), Some(secret), token.as_ref()]
            .into_iter()
            .flatten()
        {
            if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
                return Err(Error::Secret);
            }
        }
        let now = Timestamp::now();
        let mut expires = now + Duration::from_secs(self.refresh_seconds + 120);
        if let Some(value) = values.get("expiration") {
            let actual: Timestamp = value.parse().map_err(|_| Error::Secret)?;
            if actual <= now {
                return Err(Error::Secret);
            }
            expires = expires.min(actual);
        }
        Ok(Credential {
            access_key_id: access.clone(),
            secret_access_key: secret.clone(),
            session_token: token,
            expires_in: Some(expires),
        })
    }
}
fn parse(text: &str, profile: &str) -> Result<BTreeMap<String, String>, Error> {
    let mut selected = false;
    let mut found = false;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            let name = line
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .ok_or(Error::Secret)?
                .trim();
            selected = name == profile || name.strip_prefix("profile ") == Some(profile);
            if selected {
                if found {
                    return Err(Error::Secret);
                }
                found = true;
            }
            continue;
        }
        if !selected {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(Error::Secret)?;
        let key = key.trim();
        if !matches!(
            key,
            "aws_access_key_id" | "aws_secret_access_key" | "aws_session_token" | "expiration"
        ) || values.insert(key.into(), value.trim().into()).is_some()
        {
            return Err(Error::Secret);
        }
    }
    if !found {
        return Err(Error::Secret);
    }
    Ok(values)
}
impl ProvideCredential for Config {
    type Credential = Credential;
    async fn provide_credential(&self, _: &Context) -> reqsign_core::Result<Option<Credential>> {
        let file = self.clone();
        tokio::task::spawn_blocking(move || file.read())
            .await
            .ok()
            .and_then(Result::ok)
            .map(Some)
            .ok_or_else(|| {
                reqsign_core::Error::credential_invalid(
                    "storage credential file unavailable or invalid",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqsign_aws_v4::RequestSigner;
    use reqsign_core::Signer;
    fn write(path: &std::path::Path, text: &str) {
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    fn content(key: &str) -> String {
        format!("[other]\naws_access_key_id=wrong\naws_secret_access_key=wrong\n[publisher]\naws_access_key_id={key}\naws_secret_access_key=synthetic-secret\naws_session_token=synthetic-token\n")
    }
    #[tokio::test]
    async fn shared_file_rotates_cached_signer_and_never_uses_invalid_or_other_profile() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("credentials");
        write(&path, &content("FIRSTKEY"));
        let config = Config {
            path: path.clone(),
            profile: "publisher".into(),
            refresh_seconds: 1,
        };
        let signer = Signer::new(
            Context::new(),
            config,
            RequestSigner::new("s3", "us-east-1"),
        );
        async fn sign(signer: &Signer<Credential>) -> Result<String, reqsign_core::Error> {
            let mut request = http::Request::get("https://bucket.example/object")
                .body(())
                .unwrap()
                .into_parts()
                .0;
            signer.sign(&mut request, None).await?;
            assert_eq!(request.headers["x-amz-security-token"], "synthetic-token");
            Ok(request.headers["authorization"]
                .to_str()
                .unwrap()
                .to_owned())
        }
        assert!(sign(&signer)
            .await
            .unwrap()
            .contains("Credential=FIRSTKEY/"));
        write(&path, &content("SECONDKEY"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(sign(&signer)
            .await
            .unwrap()
            .contains("Credential=SECONDKEY/"));
        write(
            &path,
            "[other]\naws_access_key_id=wrong\naws_secret_access_key=wrong\n",
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let error = sign(&signer).await.unwrap_err().to_string();
        assert!(!error.contains("wrong") && !error.contains(root.path().to_str().unwrap()));
        write(&path, &content("RECOVEREDKEY"));
        assert!(sign(&signer)
            .await
            .unwrap()
            .contains("Credential=RECOVEREDKEY/"));
    }
    #[test]
    fn shared_file_rejects_duplicates_commands_expired_keys_and_unsafe_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("credentials");
        let config = Config {
            path: path.clone(),
            profile: "publisher".into(),
            refresh_seconds: 60,
        };
        for text in [
            content("KEY") + "aws_access_key_id=duplicate\n",
            content("KEY") + "[publisher]\n",
            content("KEY") + "credential_process=must-not-run\n",
            content("KEY") + "expiration=2000-01-01T00:00:00Z\n",
            content("KEY") + "expiration=not-a-date\n",
            "[publisher]\naws_access_key_id=only-key\n".into(),
            "X".repeat(65537),
        ] {
            write(&path, &text);
            assert!(config.read().is_err());
        }
        write(
            &path,
            &content("KEY").replace("[publisher]", "[profile publisher]"),
        );
        config.read().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(config.read().is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = root.path().join("link");
            symlink(&path, &link).unwrap();
            assert!(Config {
                path: link,
                ..config.clone()
            }
            .read()
            .is_err());
        }
    }
}
