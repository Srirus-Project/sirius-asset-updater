//! Provider-aware remote asset planning and optional prefix decryption.
use crate::{catalog::Catalog, Error};
use aes::cipher::{KeyIvInit, StreamCipher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetConfig {
    #[serde(default)]
    pub selection: crate::catalog::Selection,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub cache_directory: Option<std::path::PathBuf>,
    #[serde(default = "default_max_file")]
    pub max_file_bytes: u64,
    #[serde(default = "default_max_total")]
    pub max_total_bytes: u64,
    #[serde(default)]
    pub decrypt: Option<DecryptConfig>,
}
fn default_concurrency() -> usize {
    4
}
fn default_max_file() -> u64 {
    512 * 1024 * 1024
}
fn default_max_total() -> u64 {
    16 * 1024 * 1024 * 1024
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecryptConfig {
    pub key_hex_env: String,
    pub nonce_seed_hex_env: String,
}
impl AssetConfig {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        self.selection.validate()?;
        if self
            .cache_directory
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(Error::Config);
        }
        if !(1..=16).contains(&self.concurrency)
            || self.max_file_bytes < 8
            || self.max_file_bytes > 2 * 1024 * 1024 * 1024
            || self.max_total_bytes < self.max_file_bytes
            || self.max_total_bytes > 128 * 1024 * 1024 * 1024
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    EncryptedBundle,
    UnityBundle,
    Cri,
}
#[derive(Debug, Serialize)]
pub struct Asset {
    pub relative_path: String,
    pub provider: Provider,
    pub locations: Vec<u32>,
}
#[derive(Debug, Serialize)]
pub struct Plan {
    pub assets: Vec<Asset>,
    pub embedded_locations: usize,
    pub logical_locations: usize,
}
pub(crate) fn safe_relative(path: &str) -> bool {
    path.len() <= 2048
        && !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 255
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-()".contains(&c))
        })
}
impl Catalog {
    pub fn plan(&self, remote_dir: &str) -> Result<Plan, Error> {
        let mut assets = BTreeMap::<String, Asset>::new();
        let mut embedded_locations = 0;
        let mut logical_locations = 0;
        let prefix = format!("{remote_dir}/");
        for location in &self.locations {
            let provider = match location.provider_id.as_str() {
                "Fwk.Crypt.AssetBundleCryptProvider" => Provider::EncryptedBundle,
                "UnityEngine.ResourceManagement.ResourceProviders.AssetBundleProvider" => {
                    Provider::UnityBundle
                }
                "CriWare.Assets.CriResourceProvider" => Provider::Cri,
                "UnityEngine.ResourceManagement.ResourceProviders.BundledAssetProvider" => {
                    logical_locations += 1;
                    continue;
                }
                _ => return Err(Error::Provider),
            };
            let id = &location.internal_id;
            let path = if let Some(relative) = id.strip_prefix("{Fwk.Resource.RemoteAssetDir}/") {
                relative
            } else if let Some(relative) = id.strip_prefix(&prefix) {
                relative
            } else if let Some(relative) =
                id.strip_prefix("{UnityEngine.AddressableAssets.Addressables.RuntimePath}/")
            {
                if !safe_relative(relative) {
                    return Err(Error::AssetPath);
                }
                embedded_locations += 1;
                continue;
            } else {
                return Err(Error::AssetPath);
            };
            if !safe_relative(path) {
                return Err(Error::AssetPath);
            }
            if let Some(existing) = assets.get_mut(path) {
                if existing.provider != provider {
                    return Err(Error::Provider);
                }
                existing.locations.push(location.id);
            } else {
                assets.insert(
                    path.to_owned(),
                    Asset {
                        relative_path: path.to_owned(),
                        provider,
                        locations: vec![location.id],
                    },
                );
            }
        }
        // Remain safe on case-insensitive deployment filesystems as well as Linux.
        let folded: std::collections::BTreeSet<_> = assets
            .keys()
            .map(|path| path.to_ascii_lowercase())
            .collect();
        if folded.len() != assets.len() {
            return Err(Error::AssetPath);
        }
        for path in &folded {
            for (i, _) in path.match_indices('/') {
                if folded.contains(&path[..i]) {
                    return Err(Error::AssetPath);
                }
            }
        }
        Ok(Plan {
            assets: assets.into_values().collect(),
            embedded_locations,
            logical_locations,
        })
    }
}

pub struct BundleKey {
    key: [u8; 16],
    seed: [u8; 8],
}
impl BundleKey {
    pub fn from_hex(key: &str, seed: &str) -> Result<Self, Error> {
        let mut result = Self {
            key: [0; 16],
            seed: [0; 8],
        };
        hex::decode_to_slice(key, &mut result.key).map_err(|_| Error::Secret)?;
        hex::decode_to_slice(seed, &mut result.seed).map_err(|_| Error::Secret)?;
        Ok(result)
    }
    pub fn decrypt_prefix(&self, bytes: &mut [u8], original_basename: &str) -> Result<(), Error> {
        if !safe_relative(original_basename) || original_basename.contains('/') {
            return Err(Error::AssetPath);
        }
        if bytes.starts_with(b"UnityFS\0") {
            return Ok(());
        }
        let mut digest = Sha256::new();
        digest.update(self.seed);
        digest.update(original_basename.as_bytes());
        let hash = digest.finalize();
        let mut counter = [0u8; 16];
        counter[..8].copy_from_slice(&hash[..8]);
        let mut cipher = ctr::Ctr64BE::<aes::Aes128>::new(&self.key.into(), &counter.into());
        let len = bytes.len().min(16384);
        cipher.apply_keystream(&mut bytes[..len]);
        if !bytes.starts_with(b"UnityFS\0") {
            return Err(Error::Bundle);
        }
        Ok(())
    }
    pub(crate) async fn decrypt_file(
        &self,
        path: &Path,
        original_basename: &str,
    ) -> Result<bool, Error> {
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .map_err(|_| Error::Io)?;
        let mut prefix = vec![0; 16384];
        let mut len = 0;
        while len < prefix.len() {
            let count = file.read(&mut prefix[len..]).await.map_err(|_| Error::Io)?;
            if count == 0 {
                break;
            }
            len += count;
        }
        prefix.truncate(len);
        let already_plain = prefix.starts_with(b"UnityFS\0");
        self.decrypt_prefix(&mut prefix, original_basename)?;
        if already_plain {
            return Ok(false);
        }
        file.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|_| Error::Io)?;
        file.write_all(&prefix).await.map_err(|_| Error::Io)?;
        file.sync_all().await.map_err(|_| Error::Io)?;
        Ok(true)
    }
}
