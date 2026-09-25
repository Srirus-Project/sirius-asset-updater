//! Filtered publication of verified, decrypted Unity bundle bytes.
use crate::{assets::Provider, Error};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Alongside,
    Only,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub output_prefix: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Alongside,
            include: vec![],
            exclude: vec![],
            output_prefix: "raw".into(),
        }
    }
}
pub(crate) fn safe_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 2048
        && !s.contains(['\\', ':'])
        && !s.chars().any(char::is_control)
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if !safe_path(&self.output_prefix) || self.include.len() + self.exclude.len() > 128 {
            return Err(Error::Config);
        }
        for s in self.include.iter().chain(&self.exclude) {
            if s.is_empty() || s.len() > 4096 {
                return Err(Error::Config);
            }
            regex::Regex::new(s).map_err(|_| Error::Config)?;
        }
        Ok(())
    }
    pub fn matches(&self, provider: Provider, path: &str) -> bool {
        provider != Provider::Cri && self.matches_path(path)
    }
    pub(crate) fn matches_path(&self, path: &str) -> bool {
        let matches = |p: &String| regex::Regex::new(p).is_ok_and(|r| r.is_match(path));
        safe_path(path)
            && (self.include.is_empty() || self.include.iter().any(matches))
            && !self.exclude.iter().any(matches)
    }
    pub(crate) fn output_path(&self, path: &str) -> Result<String, Error> {
        if !safe_path(path)
            || !safe_path(&self.output_prefix)
            || path.len() + self.output_prefix.len() + 1 > 4096
        {
            return Err(Error::AssetPath);
        }
        Ok(format!("{}/{}", self.output_prefix, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_are_native_path_based_and_reject_unsafe_configuration() {
        let c: Config = yaml_serde::from_str("include: ['^music/']\nexclude: ['debug']").unwrap();
        c.validate().unwrap();
        assert!(c.matches(Provider::UnityBundle, "music/a.bundle"));
        assert!(c.matches(Provider::EncryptedBundle, "music/a.bundle"));
        assert!(!c.matches(Provider::Cri, "music/a.bundle"));
        assert!(!c.matches(Provider::UnityBundle, "music/debug.bundle"));
        assert!(!c.matches(Provider::UnityBundle, "other/a.bundle"));
        for path in [
            "/absolute",
            "../escape",
            "a/../b",
            "a//b",
            "a\\b",
            "",
            "x:y",
        ] {
            assert!(Config {
                output_prefix: path.into(),
                ..Default::default()
            }
            .validate()
            .is_err());
            assert!(c.output_path(path).is_err());
        }
        for yaml in ["include: ['(']", "exclude: ['']"] {
            assert!(yaml_serde::from_str::<Config>(yaml)
                .unwrap()
                .validate()
                .is_err());
        }
        assert!(yaml_serde::from_str::<Config>("mode: discard").is_err());
    }
}
