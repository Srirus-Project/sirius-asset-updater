//! Environment path overrides for YAML configuration documents.
//!
//! Restores the original `HARUKI__A__B=value` mechanism with one prefix per Sirius document
//! kind. Overrides are applied to the parsed YAML tree before typed deserialization, so every
//! overridden value still passes `deny_unknown_fields` and each document's own validation.
//! There is deliberately no `${env:VAR}` string interpolation: secrets stay behind the typed
//! `*_env` references instead of being copied into configuration snapshots and summaries.
use crate::Error;
use std::path::Path;
use yaml_serde::{Mapping, Value};

/// Which document an override prefix addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Document {
    /// Download/update configuration (`SIRIUS_ASSET_CONFIG_PATH`, service `download_config`).
    Download,
    /// Export configuration (`export CONFIG`, service `export_config`).
    Export,
    /// Service configuration (`serve CONFIG`).
    Service,
    /// Service profile `storage_config`.
    Storage,
    /// `publish`/`plan-storage` command documents.
    Publish,
    /// Remote download-configuration bootstrap (`SIRIUS_ASSET_CONFIG_SOURCE__*`); built only
    /// from the environment, never read from a file.
    ConfigSource,
}
impl Document {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Download => "SIRIUS_ASSET__",
            Self::Export => "SIRIUS_ASSET_EXPORT__",
            Self::Service => "SIRIUS_ASSET_SERVICE__",
            Self::Storage => "SIRIUS_ASSET_STORAGE__",
            Self::Publish => "SIRIUS_ASSET_PUBLISH__",
            Self::ConfigSource => "SIRIUS_ASSET_CONFIG_SOURCE__",
        }
    }
}

const MAX_OVERRIDES: usize = 256;
const MAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 16;
/// Bounds sequence growth so one variable cannot request an enormous allocation.
const MAX_INDEX: usize = 1024;

/// Read `path`, apply this process's overrides for `document`, then deserialize.
pub fn load<T: serde::de::DeserializeOwned>(path: &Path, document: Document) -> Result<T, Error> {
    from_str(
        &std::fs::read_to_string(path).map_err(|_| Error::Config)?,
        document,
    )
}
pub fn from_str<T: serde::de::DeserializeOwned>(
    text: &str,
    document: Document,
) -> Result<T, Error> {
    from_str_with(text, document, &process_vars())
}
/// UTF-8 environment snapshot; non-UTF-8 names or values cannot address a document.
pub(crate) fn process_vars() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}
pub(crate) fn from_str_with<T: serde::de::DeserializeOwned>(
    text: &str,
    document: Document,
    vars: &[(String, String)],
) -> Result<T, Error> {
    let mut root: Value = yaml_serde::from_str(text).map_err(|_| Error::Config)?;
    apply(&mut root, document, vars)?;
    yaml_serde::from_value(root).map_err(|_| Error::Config)
}
/// Apply overrides in name order so the result does not depend on environment ordering.
pub(crate) fn apply(
    root: &mut Value,
    document: Document,
    vars: &[(String, String)],
) -> Result<(), Error> {
    let mut selected: Vec<(&str, &str)> = vars
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix(document.prefix())?, v.as_str())))
        .collect();
    if selected.len() > MAX_OVERRIDES {
        return Err(Error::Config);
    }
    selected.sort_unstable();
    for (path, raw) in selected {
        let segments: Vec<String> = path.split("__").map(str::to_ascii_lowercase).collect();
        if segments.len() > MAX_DEPTH
            || raw.len() > MAX_VALUE_BYTES
            || segments
                .iter()
                .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err(Error::Config);
        }
        set(root, &segments, value(raw))?;
    }
    Ok(())
}
/// Same scalar rules as the original: YAML when it parses, otherwise the literal string.
fn value(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::String(String::new());
    }
    yaml_serde::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}
fn set(current: &mut Value, path: &[String], value: Value) -> Result<(), Error> {
    let Some((segment, rest)) = path.split_first() else {
        *current = value;
        return Ok(());
    };
    if let Ok(index) = segment.parse::<usize>() {
        if index > MAX_INDEX {
            return Err(Error::Config);
        }
        if current.is_null() {
            *current = Value::Sequence(Vec::new());
        }
        let Value::Sequence(items) = current else {
            return Err(Error::Config);
        };
        if items.len() <= index {
            items.resize(index + 1, Value::Null);
        }
        return set(&mut items[index], rest, value);
    }
    if current.is_null() {
        *current = Value::Mapping(Mapping::new());
    }
    let Value::Mapping(map) = current else {
        return Err(Error::Config);
    };
    let key = Value::String(segment.clone());
    set(map.entry(key).or_insert(Value::Null), rest, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn applied(text: &str, document: Document, pairs: &[(&str, &str)]) -> Result<Value, Error> {
        let mut root: Value = yaml_serde::from_str(text).unwrap();
        apply(&mut root, document, &vars(pairs))?;
        Ok(root)
    }
    #[test]
    fn overrides_nested_fields_types_sequences_and_new_mappings() {
        let root = applied(
            "a: {b: 1, list: [x, y]}\nkeep: true\n",
            Document::Download,
            &[
                ("SIRIUS_ASSET__A__B", "42"),
                ("SIRIUS_ASSET__A__LIST__1", "z"),
                ("SIRIUS_ASSET__A__LIST__3", "false"),
                ("SIRIUS_ASSET__NEW__NESTED", "[1, 2]"),
                ("SIRIUS_ASSET__TEXT", "not: valid: yaml: :"),
                ("SIRIUS_ASSET__EMPTY", ""),
                // Other document kinds and unrelated names are ignored.
                ("SIRIUS_ASSET_EXPORT__A__B", "7"),
                ("SIRIUS_ASSET_CONFIG_PATH", "/elsewhere"),
            ],
        )
        .unwrap();
        let expected: Value = yaml_serde::from_str(
            "a: {b: 42, list: [x, z, null, false]}\nkeep: true\nnew: {nested: [1, 2]}\ntext: 'not: valid: yaml: :'\nempty: ''\n",
        )
        .unwrap();
        assert_eq!(root, expected);
        let export = applied(
            "a: {b: 1}",
            Document::Export,
            &[("SIRIUS_ASSET_EXPORT__A__B", "7")],
        )
        .unwrap();
        assert_eq!(export["a"]["b"], Value::from(7));
    }
    #[test]
    fn rejects_malformed_or_unbounded_overrides() {
        for name in [
            "SIRIUS_ASSET__",
            "SIRIUS_ASSET__A____B",
            "SIRIUS_ASSET__A-B",
            "SIRIUS_ASSET__LIST__1025",
        ] {
            assert!(
                applied("a: 1", Document::Download, &[(name, "1")]).is_err(),
                "{name}"
            );
        }
        // Descending through a scalar or indexing a mapping is a type error, not a silent replace.
        assert!(applied("a: 1", Document::Download, &[("SIRIUS_ASSET__A__B", "1")]).is_err());
        assert!(applied(
            "a: {b: 1}",
            Document::Download,
            &[("SIRIUS_ASSET__A__0", "1")]
        )
        .is_err());
        let deep = format!("SIRIUS_ASSET{}", "__A".repeat(17));
        assert!(applied("{}", Document::Download, &[(&deep, "1")]).is_err());
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        assert!(applied("{}", Document::Download, &[("SIRIUS_ASSET__A", &big)]).is_err());
        let many: Vec<(String, String)> = (0..=MAX_OVERRIDES)
            .map(|i| (format!("SIRIUS_ASSET__K{i}"), "1".into()))
            .collect();
        let mut root: Value = yaml_serde::from_str("{}").unwrap();
        assert!(apply(&mut root, Document::Download, &many).is_err());
    }
    #[test]
    fn typed_documents_still_reject_unknown_fields_after_overrides() {
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        struct Typed {
            #[allow(dead_code)]
            count: u32,
        }
        let ok: Typed = from_str_with(
            "count: 1",
            Document::Service,
            &vars(&[("SIRIUS_ASSET_SERVICE__COUNT", "5")]),
        )
        .unwrap();
        assert_eq!(ok.count, 5);
        assert!(from_str_with::<Typed>(
            "count: 1",
            Document::Service,
            &vars(&[("SIRIUS_ASSET_SERVICE__COUNTS", "5")])
        )
        .is_err());
        assert!(from_str_with::<Typed>(
            "count: 1",
            Document::Service,
            &vars(&[("SIRIUS_ASSET_SERVICE__COUNT", "many")])
        )
        .is_err());
    }
}
