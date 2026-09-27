//! Stable, human-readable storage keys for an export (`layout: stable`).
//!
//! The export itself keeps numbered resource directories (names from game data never become
//! local filesystem paths). This module maps each verified output to the key a developer would
//! look for, following the original Haruki updater's `by_category` export
//! (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/paths.rs`):
//!
//! - Unity objects: the container path without the configured prefix, with the output extension
//!   (`Assets/AddressableResources/Adv/x/back.png` -> `adv/x/back.png`). Other objects of the same
//!   container go to `<container stem>.assets/<type dir>/<object stem>.<ext>`; a MonoBehaviour
//!   named like its container and the single Texture2D named like its container stay flat;
//!   fonts sit next to the container. Objects without a container go to
//!   `_bundles/<bundle>/<type dir>/`.
//! - CRI resources (`cri_assets_cri/...`, hash suffix removed): ACB tracks are named by cue in a
//!   directory per ACB, USM outputs sit next to each other as `<stem>.<ext>`.
//! - Collisions resolve in catalog order: byte-identical content is written once, different
//!   content gets `__dup2`, `__dup3`, ... (the original resolved in processing order).
//!
//! Every key is re-checked with the same rules as local export paths before use.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait, JsonValueTrait};

use crate::export::{ObjectIdentity, OutputRecord, ResourceReport};
use crate::Error;

const MAX_STEM_CHARS: usize = 220;
const DEFAULT_STRIP_PREFIXES: [&str; 1] = ["Assets/AddressableResources"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// Container prefixes removed before building keys; the first exact (case-sensitive) match
    /// wins and a container that would become empty keeps its full path.
    #[serde(default = "default_strip_prefixes")]
    pub strip_prefixes: Vec<String>,
    /// Lowercase every key (Unity containers mix case; lowercase keys cannot collide on
    /// case-insensitive consumers).
    #[serde(default = "default_true")]
    pub lowercase: bool,
    /// Delete keys listed in the previous manifest that this export no longer produces.
    #[serde(default)]
    pub prune: bool,
}
fn default_strip_prefixes() -> Vec<String> {
    DEFAULT_STRIP_PREFIXES
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_true() -> bool {
    true
}
impl Default for Options {
    fn default() -> Self {
        Self {
            strip_prefixes: default_strip_prefixes(),
            lowercase: true,
            prune: false,
        }
    }
}
impl Options {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.strip_prefixes.len() > 16
            || self
                .strip_prefixes
                .iter()
                .any(|p| p.trim_matches('/').is_empty() || p.len() > 256 || p.contains('\\'))
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}

/// One file to publish: the verified local export path and its stable key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub local: String,
    pub bytes: u64,
    pub sha256: String,
    pub source: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub class_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Unique keys in catalog order.
    pub entries: Vec<Entry>,
    /// Outputs whose content equals an already planned key (not uploaded twice).
    pub deduplicated: usize,
    /// Outputs that needed a `__dupN` suffix.
    pub renamed: usize,
}

/// Unity class names for the IDs seen in Sirius bundles and the common built-in types.
fn class_name(id: i32) -> Option<&'static str> {
    Some(match id {
        1 => "GameObject",
        2 => "Component",
        4 => "Transform",
        8 => "Behaviour",
        20 => "Camera",
        21 => "Material",
        23 => "MeshRenderer",
        25 => "Renderer",
        28 => "Texture2D",
        33 => "MeshFilter",
        43 => "Mesh",
        48 => "Shader",
        49 => "TextAsset",
        50 => "Rigidbody2D",
        54 => "Rigidbody",
        56 => "Collider",
        61 => "BoxCollider2D",
        64 => "MeshCollider",
        65 => "BoxCollider",
        74 => "AnimationClip",
        81 => "AudioListener",
        82 => "AudioSource",
        83 => "AudioClip",
        84 => "RenderTexture",
        89 => "Cubemap",
        90 => "Avatar",
        91 => "AnimatorController",
        95 => "Animator",
        96 => "TrailRenderer",
        102 => "TextMesh",
        108 => "Light",
        111 => "Animation",
        114 => "MonoBehaviour",
        115 => "MonoScript",
        117 => "Texture3D",
        120 => "LineRenderer",
        128 => "Font",
        135 => "SphereCollider",
        136 => "CapsuleCollider",
        137 => "SkinnedMeshRenderer",
        142 => "AssetBundle",
        152 => "MovieTexture",
        187 => "Texture2DArray",
        198 => "ParticleSystem",
        199 => "ParticleSystemRenderer",
        205 => "LODGroup",
        210 => "SortingGroup",
        212 => "SpriteRenderer",
        213 => "Sprite",
        221 => "AnimatorOverrideController",
        222 => "CanvasRenderer",
        223 => "Canvas",
        224 => "RectTransform",
        225 => "CanvasGroup",
        290 => "AssetBundleManifest",
        320 => "PlayableDirector",
        328 => "VideoPlayer",
        329 => "VideoClip",
        331 => "SpriteMask",
        687078895 => "SpriteAtlas",
        _ => return None,
    })
}

/// The original semantic sub-directories (`paths.rs:62-99`), plus the snake_case class name for
/// types the original could not name.
fn semantic_dir(class: &str) -> String {
    match class.to_ascii_lowercase().as_str() {
        "texture2darray" => "texture2d_array".into(),
        "shadervariantcollection" => "shader".into(),
        "sprite" | "mesh" | "animator" | "monobehaviour" | "monoscript" | "gameobject"
        | "material" | "transform" | "recttransform" | "canvas" | "camera" | "avatar"
        | "animation" | "cubemap" | "texture3d" | "shader" | "texture2d" => {
            class.to_ascii_lowercase()
        }
        _ => snake_case(class),
    }
}
fn snake_case(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if i > 0 && (prev_lower || (next_lower && chars[i - 1].is_ascii_uppercase())) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// `assetstudio_fix_file_name` (`paths.rs:403-437`) plus removal of path separators.
fn fix_file_name(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|ch| match ch {
            // The original's set plus `#` and `%`, which would break the key inside a URL.
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '#' | '%' => '_',
            _ if ch.is_control() => '_',
            _ => ch,
        })
        .collect();
    let safe = compress_clone_suffixes(safe.trim());
    if safe.chars().count() <= MAX_STEM_CHARS {
        return safe;
    }
    let keep = MAX_STEM_CHARS - "__truncated".len();
    let mut short: String = safe.chars().take(keep).collect();
    short.push_str("__truncated");
    short
}
fn compress_clone_suffixes(value: &str) -> String {
    let marker = "(Clone)";
    let mut end = value.len();
    let mut count = 0;
    while end >= marker.len() && value[..end].ends_with(marker) {
        end -= marker.len();
        count += 1;
    }
    if count <= 1 {
        return value.to_string();
    }
    format!("{}__clone{count}", value[..end].trim_end())
}
/// `normalize_semantic_path_component` (`paths.rs:130-137`).
fn loose(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| *c != '_' && *c != '-' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Container without the first matching prefix, normal components only (`paths.rs:455-479`).
fn strip_container(container: &str, prefixes: &[String]) -> Vec<String> {
    let normalized = container.replace('\\', "/");
    let normalized = normalized.trim_start_matches('/');
    let stripped = prefixes
        .iter()
        .find_map(|p| {
            normalized
                .strip_prefix(p.trim_matches('/'))
                .filter(|rest| rest.starts_with('/'))
                .map(|rest| rest.trim_start_matches('/'))
                .filter(|rest| !rest.is_empty())
        })
        .unwrap_or(normalized);
    stripped
        .split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(|c| {
            c.chars()
                .map(|ch| if ch.is_control() { '_' } else { ch })
                .collect()
        })
        .collect()
}

fn split_ext(file: &str) -> (&str, &str) {
    match file.rfind('.') {
        Some(0) | None => (file, ""),
        Some(i) => (&file[..i], &file[i + 1..]),
    }
}
/// `Path::set_extension` on the last component.
fn with_extension(file: &str, ext: &str) -> String {
    let (stem, _) = split_ext(file);
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// Addressables key `x_assets_x/dir/name_<32 hex>` -> (`x/dir`, `name`), e.g.
/// `cri_assets_cri/sound/voice_<hash>` -> (`cri/sound`, `voice`).
fn cri_location(source: &str) -> (Vec<String>, String) {
    let mut parts: Vec<String> = source
        .split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(fix_file_name)
        .collect();
    if parts.len() > 1 {
        if let Some((group, rest)) = parts[0].split_once("_assets_") {
            if group == rest && !group.is_empty() {
                parts[0] = group.to_string();
            }
        }
    }
    let last = parts.pop().unwrap_or_else(|| "resource".into());
    let stem = match last.rsplit_once('_') {
        Some((stem, hash))
            if !stem.is_empty()
                && hash.len() == 32
                && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            stem.to_string()
        }
        _ => last,
    };
    (parts, stem)
}

fn output_file(output: &OutputRecord) -> &str {
    output.path.rsplit('/').next().unwrap_or(&output.path)
}
fn output_ext(output: &OutputRecord) -> &str {
    split_ext(output_file(output)).1
}

struct Context<'a> {
    options: &'a Options,
    /// (stripped container key) -> number of Texture2D objects and whether the single one
    /// is named like the container.
    textures: HashMap<String, (usize, bool)>,
    unnamed: HashMap<(String, i32), usize>,
    /// Stem already chosen for an unnamed object, so all outputs of the object agree.
    stems: HashMap<(String, i64), String>,
}

impl Context<'_> {
    fn object_stem(&mut self, object: &ObjectIdentity, scope: &str, class: &str) -> String {
        if let Some(name) = object
            .name
            .as_deref()
            .map(fix_file_name)
            .filter(|n| !n.is_empty())
        {
            return name;
        }
        let identity = (object.source_file.clone(), object.path_id);
        if let Some(stem) = self.stems.get(&identity) {
            return stem.clone();
        }
        let counter = self
            .unnamed
            .entry((scope.to_string(), object.class_id))
            .or_insert(0);
        // The original used `{Type}_#{index}`; `#` would start a URL fragment.
        let stem = format!("{class}_{counter}");
        *counter += 1;
        self.stems.insert(identity, stem.clone());
        stem
    }

    fn unity_key(
        &mut self,
        resource: &ResourceReport,
        output: &OutputRecord,
        object: &ObjectIdentity,
    ) -> Vec<String> {
        let class = class_name(object.class_id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("ClassID{}", object.class_id));
        let ext = output_ext(output).to_string();
        let container = object.container.as_deref().filter(|c| !c.trim().is_empty());
        let Some(container) = container else {
            // No container: keep objects of one bundle together instead of the original's
            // flat `<name>.<ext>` at the export root.
            let (_, bundle) = cri_location(resource.source.trim_end_matches(".bundle"));
            let scope = format!("_bundles/{bundle}");
            let stem = self.object_stem(object, &scope, &class);
            return vec![
                "_bundles".into(),
                bundle,
                semantic_dir(&class),
                join_ext(&stem, &ext),
            ];
        };
        let mut parts = strip_container(container, &self.options.strip_prefixes);
        let Some(file) = parts.pop() else {
            return vec![join_ext(&fix_file_name(&class), &ext)];
        };
        let scope = container_key(&parts, &file);
        let (container_stem, container_ext) = split_ext(&file);
        let container_stem = container_stem.to_string();
        let flat = |ext: &str| {
            let mut p = parts.clone();
            p.push(with_extension(&file, ext));
            p
        };
        let nested = |this: &mut Self, dir: &str| {
            let stem = this.object_stem(object, &scope, &class);
            let mut p = parts.clone();
            p.push(format!("{container_stem}.assets"));
            p.push(dir.to_string());
            p.push(join_ext(&stem, &ext));
            p
        };
        match object.class_id {
            // TextAsset: the raw bytes of the container file. `x.acb.bytes` -> `x.acb`,
            // `x.bytes` -> `x`, other container extensions are kept (`paths` rewrite in
            // `payload/naming.rs:33-59`, without turning `foo.txt` into `foo`).
            // Several TextAssets in one container (e.g. `x.asset` holding `x-002`, `x-003`, ...)
            // are named like MonoBehaviours instead of all claiming the container path.
            49 if object
                .name
                .as_deref()
                .is_some_and(|n| loose(&fix_file_name(n)) != loose(&container_stem))
                && !container_ext.eq_ignore_ascii_case("bytes") =>
            {
                nested(self, "text_asset")
            }
            49 => {
                if container_ext.eq_ignore_ascii_case("bytes") {
                    let mut p = parts.clone();
                    p.push(container_stem);
                    p
                } else if container_ext.is_empty() {
                    flat(&ext)
                } else {
                    let mut p = parts.clone();
                    p.push(file.clone());
                    p
                }
            }
            // MonoBehaviour named like its container stays flat (`paths.rs:107-128`).
            114 if object
                .name
                .as_deref()
                .is_some_and(|n| loose(&fix_file_name(n)) == loose(&container_stem)) =>
            {
                flat(&ext)
            }
            // Texture2D: the single texture named like its container stays flat, every other
            // texture of the container is named (`paths.rs:526-573`).
            28 => match self.textures.get(&scope) {
                Some((1, true)) => flat(&ext),
                _ => nested(self, "texture2d"),
            },
            // Fonts sit next to the container under their own name (`paths.rs:270-281`).
            128 => {
                let stem = self.object_stem(object, &scope, &class);
                let mut p = parts.clone();
                p.push(join_ext(&stem, &ext));
                p
            }
            // Leaf assets keep the container path.
            83 | 329 | 152 | 687078895 => flat(&ext),
            _ => match class_name(object.class_id) {
                Some(_) => nested(self, &semantic_dir(&class)),
                None => flat(&ext),
            },
        }
    }

    fn cri_key(
        &self,
        resource: &ResourceReport,
        output: &OutputRecord,
        cue: Option<&str>,
    ) -> Vec<String> {
        let (mut parts, stem) = cri_location(&resource.source);
        let file = output_file(output);
        let (base, ext) = split_ext(file);
        let index = file.split('.').next().unwrap_or(file);
        let numbered = index.len() == 5 && index.bytes().all(|b| b.is_ascii_digit());
        if let Some(cue) = cue {
            // ACB: one directory per ACB, one file per cue (`<cue>.wav`, `<cue>.cues.json`).
            let rest = file.split_once('.').map_or(ext, |x| x.1);
            parts.push(stem);
            parts.push(join_ext(cue, rest));
            return parts;
        }
        // USM and other media: `<dir of the source>/<stem>.<ext>` next to each other.
        let name = if numbered {
            join_ext(&stem, file.split_once('.').map_or(ext, |x| x.1))
        } else if base == "movie" {
            join_ext(&stem, ext)
        } else {
            format!("{stem}.{file}")
        };
        parts.push(stem);
        parts.push(name);
        parts
    }
}
fn join_ext(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}
fn container_key(parts: &[String], file: &str) -> String {
    let mut key = parts.join("/");
    if !key.is_empty() {
        key.push('/');
    }
    key.push_str(file);
    key.to_lowercase()
}

/// ACB track naming for an output: `None` when the output is not an ACB track, otherwise the
/// first cue name of its `NNNNN.cues.json` (`[[name, cue_id, ...], ...]`) or, for tracks without
/// a cue, the track index.
fn acb_track(export: &Path, resource: &ResourceReport, output: &OutputRecord) -> Option<String> {
    let (dir, file) = match output.path.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, output.path.as_str()),
    };
    let index = file.split('.').next()?;
    if index.len() != 5 || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let cues = match dir {
        Some(dir) => format!("{dir}/{index}.cues.json"),
        None => format!("{index}.cues.json"),
    };
    if !resource
        .outputs
        .iter()
        .any(|o| o.kind == "cue_metadata" && o.path == cues)
    {
        return None;
    }
    let cue = std::fs::read(export.join(&resource.output_directory).join(&cues))
        .ok()
        .filter(|bytes| bytes.len() <= 1024 * 1024)
        .and_then(|bytes| sonic_rs::from_slice::<sonic_rs::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .as_array()?
                .first()?
                .as_array()?
                .first()?
                .as_str()
                .map(fix_file_name)
        })
        .filter(|name| !name.is_empty());
    Some(cue.unwrap_or_else(|| index.to_string()))
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 1024
        && !key.contains(['\\', '\0'])
        && !key.chars().any(char::is_control)
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && part.len() <= 255)
}

/// Plan stable keys for a verified export directory (its `resources.jsonl` in catalog order).
pub fn plan(export: &Path, options: &Options) -> Result<Plan, Error> {
    use std::io::BufRead;
    let journal = std::fs::File::open(export.join("resources.jsonl")).map_err(|_| Error::Io)?;
    let mut resources = Vec::new();
    for line in std::io::BufReader::new(journal).lines() {
        let line = line.map_err(|_| Error::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        let resource: ResourceReport =
            sonic_rs::from_str(&line).map_err(|_| Error::Verification)?;
        resources.push(resource);
    }
    plan_resources(export, &resources, options)
}

pub(crate) fn plan_resources(
    export: &Path,
    resources: &[ResourceReport],
    options: &Options,
) -> Result<Plan, Error> {
    let mut textures: HashMap<String, (usize, bool)> = HashMap::new();
    for resource in resources {
        let mut seen = HashSet::new();
        for output in &resource.outputs {
            let Some(object) = &output.object else {
                continue;
            };
            if object.class_id != 28 || !seen.insert((object.source_file.clone(), object.path_id)) {
                continue;
            }
            let Some(container) = object.container.as_deref().filter(|c| !c.trim().is_empty())
            else {
                continue;
            };
            let mut parts = strip_container(container, &options.strip_prefixes);
            let Some(file) = parts.pop() else { continue };
            let named_like = object
                .name
                .as_deref()
                .is_some_and(|n| fix_file_name(n) == fix_file_name(split_ext(&file).0));
            let entry = textures
                .entry(container_key(&parts, &file))
                .or_insert((0, false));
            entry.0 += 1;
            entry.1 = entry.0 == 1 && named_like;
        }
    }
    let mut context = Context {
        options,
        textures,
        unnamed: HashMap::new(),
        stems: HashMap::new(),
    };
    let mut claimed: HashMap<String, usize> = HashMap::new();
    let mut lowered: HashMap<String, String> = HashMap::new();
    let mut plan = Plan::default();
    for resource in resources {
        if !resource.errors.is_empty() {
            return Err(Error::Verification);
        }
        for output in &resource.outputs {
            let parts = match &output.object {
                Some(object) if output.path.contains('/') => {
                    let mut parts = context.unity_key(resource, output, object);
                    let file = output_file(output);
                    let rest = file.split_once('.').map_or("", |x| x.1).to_string();
                    let name = acb_track(export, resource, output)
                        .unwrap_or_else(|| file.split('.').next().unwrap_or(file).to_string());
                    if let Some(last) = parts.pop() {
                        parts.push(split_ext(&last).0.to_string());
                    }
                    parts.push(join_ext(&name, &rest));
                    parts
                }
                Some(object) => context.unity_key(resource, output, object),
                None => {
                    let track = acb_track(export, resource, output);
                    context.cri_key(resource, output, track.as_deref())
                }
            };
            let mut key = parts.join("/");
            if options.lowercase {
                key = key.to_lowercase();
            }
            if !valid_key(&key) {
                return Err(Error::Verification);
            }
            let local = format!("{}/{}", resource.output_directory, output.path);
            let entry = |path: String| Entry {
                path,
                local: local.clone(),
                bytes: output.bytes,
                sha256: output.sha256.clone(),
                source: resource.source.clone(),
                kind: output.kind.clone(),
                class_id: output.object.as_ref().map(|o| o.class_id),
                container: output.object.as_ref().and_then(|o| o.container.clone()),
                name: output.object.as_ref().and_then(|o| o.name.clone()),
            };
            // Compare case-insensitively so keys stay unique for case-insensitive consumers
            // even with `lowercase: false`.
            let mut candidate = key.clone();
            let mut n = 1;
            loop {
                let folded = candidate.to_lowercase();
                match lowered.get(&folded) {
                    None => {
                        if n > 1 {
                            plan.renamed += 1;
                        }
                        lowered.insert(folded, candidate.clone());
                        claimed.insert(candidate.clone(), plan.entries.len());
                        plan.entries.push(entry(candidate));
                        break;
                    }
                    Some(existing) => {
                        let index = claimed[existing];
                        if plan.entries[index].sha256 == output.sha256
                            && plan.entries[index].bytes == output.bytes
                        {
                            plan.deduplicated += 1;
                            break;
                        }
                        n += 1;
                        let (stem, ext) = match key.rsplit_once('/') {
                            Some((dir, file)) => {
                                let (s, e) = split_ext(file);
                                (format!("{dir}/{s}"), e.to_string())
                            }
                            None => {
                                let (s, e) = split_ext(&key);
                                (s.to_string(), e.to_string())
                            }
                        };
                        candidate = join_ext(&format!("{stem}__dup{n}"), &ext);
                    }
                }
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(
        path: &str,
        kind: &str,
        sha: &str,
        object: Option<(i32, Option<&str>, Option<&str>)>,
    ) -> OutputRecord {
        OutputRecord {
            path: path.into(),
            kind: kind.into(),
            bytes: 1,
            sha256: sha.repeat(64 / sha.len().max(1))[..64].to_string(),
            object: object.map(|(class_id, name, container)| ObjectIdentity {
                source_file: "b.bundle::CAB".into(),
                path_id: path
                    .trim_start_matches("0_")
                    .split('.')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
                class_id,
                name: name.map(str::to_string),
                container: container.map(str::to_string),
            }),
        }
    }
    fn res(source: &str, dir: &str, outputs: Vec<OutputRecord>) -> ResourceReport {
        ResourceReport {
            source: source.into(),
            output_directory: dir.into(),
            outputs,
            ..Default::default()
        }
    }
    fn keys(plan: &Plan) -> Vec<&str> {
        plan.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn unity_objects_follow_the_original_container_layout() {
        let c = "Assets/AddressableResources/Adv/Chat/Data/003_rana/data/back.png";
        let prefab = "Assets/AddressableResources/UI/Home/HomeButton.prefab";
        let r = res(
            "adv_bundle_0123456789abcdef0123456789abcdef",
            "00000",
            vec![
                out(
                    "0_1.png",
                    "image_png",
                    "a",
                    Some((28, Some("back"), Some(c))),
                ),
                out(
                    "0_2.png",
                    "image_png",
                    "b",
                    Some((213, Some("back"), Some(c))),
                ),
                out(
                    "0_3.json",
                    "typetree_json",
                    "c",
                    Some((1, Some("HomeButton"), Some(prefab))),
                ),
                out(
                    "0_4.json",
                    "typetree_json",
                    "d",
                    Some((114, Some("HomeButton"), Some(prefab))),
                ),
                out(
                    "0_5.json",
                    "typetree_json",
                    "e",
                    Some((114, None, Some(prefab))),
                ),
                out(
                    "0_6.json",
                    "typetree_json",
                    "f",
                    Some((114, None, Some(prefab))),
                ),
                out(
                    "0_7.json",
                    "typetree_json",
                    "g",
                    Some((4, None, Some(prefab))),
                ),
                out(
                    "0_8.json",
                    "typetree_json",
                    "h",
                    Some((224, None, Some(prefab))),
                ),
                out(
                    "0_9.json",
                    "typetree_json",
                    "i",
                    Some((142, Some("x.bundle"), None)),
                ),
            ],
        );
        let plan = plan_resources(Path::new("/nonexistent"), &[r], &Options::default()).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "adv/chat/data/003_rana/data/back.png",
                "adv/chat/data/003_rana/data/back.assets/sprite/back.png",
                "ui/home/homebutton.assets/gameobject/homebutton.json",
                "ui/home/homebutton.json",
                "ui/home/homebutton.assets/monobehaviour/monobehaviour_0.json",
                "ui/home/homebutton.assets/monobehaviour/monobehaviour_1.json",
                "ui/home/homebutton.assets/transform/transform_0.json",
                "ui/home/homebutton.assets/recttransform/recttransform_0.json",
                "_bundles/adv_bundle/asset_bundle/x.bundle.json",
            ]
        );
    }

    #[test]
    fn texture_plan_text_assets_and_prefix_rules() {
        let atlas = "Assets/AddressableResources/Ui/Atlas.prefab";
        let r = res(
            "b",
            "00001",
            vec![
                // Two textures referenced by one prefab are both named.
                out(
                    "0_1.png",
                    "image_png",
                    "a",
                    Some((28, Some("btn_a"), Some(atlas))),
                ),
                out(
                    "0_2.png",
                    "image_png",
                    "b",
                    Some((28, Some("btn_b"), Some(atlas))),
                ),
                out(
                    "0_3.bytes",
                    "text_bytes",
                    "c",
                    Some((
                        49,
                        Some("story"),
                        Some("Assets/AddressableResources/Story/story.txt"),
                    )),
                ),
                out(
                    "0_4.bytes",
                    "text_bytes",
                    "d",
                    Some((
                        49,
                        Some("x"),
                        Some("Assets/AddressableResources/Sound/x.acb.bytes"),
                    )),
                ),
                // Outside the stripped prefix and with traversal components.
                out(
                    "0_5.json",
                    "typetree_json",
                    "e",
                    Some((114, Some("Pkg"), Some("Packages/com.x/../Pkg.asset"))),
                ),
            ],
        );
        let plan = plan_resources(Path::new("/nonexistent"), &[r], &Options::default()).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "ui/atlas.assets/texture2d/btn_a.png",
                "ui/atlas.assets/texture2d/btn_b.png",
                "story/story.txt",
                "sound/x.acb",
                "packages/com.x/pkg.json",
            ]
        );
    }

    #[test]
    fn collisions_dedupe_identical_content_and_suffix_different_content() {
        let c = "Assets/AddressableResources/A/B.prefab";
        let r = res(
            "b",
            "00000",
            vec![
                out(
                    "0_1.json",
                    "typetree_json",
                    "a",
                    Some((1, Some("Same"), Some(c))),
                ),
                out(
                    "0_2.json",
                    "typetree_json",
                    "a",
                    Some((1, Some("Same"), Some(c))),
                ),
                out(
                    "0_3.json",
                    "typetree_json",
                    "b",
                    Some((1, Some("Same"), Some(c))),
                ),
                out(
                    "0_4.json",
                    "typetree_json",
                    "c",
                    Some((1, Some("SAME"), Some(c))),
                ),
            ],
        );
        let plan = plan_resources(Path::new("/nonexistent"), &[r], &Options::default()).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "a/b.assets/gameobject/same.json",
                "a/b.assets/gameobject/same__dup2.json",
                "a/b.assets/gameobject/same__dup3.json",
            ]
        );
        assert_eq!((plan.deduplicated, plan.renamed), (1, 2));
        // Case-folded collisions are resolved even when keys keep their case.
        let keep_case = Options {
            lowercase: false,
            ..Options::default()
        };
        let r = res(
            "b",
            "00000",
            vec![
                out(
                    "0_1.json",
                    "typetree_json",
                    "a",
                    Some((1, Some("Same"), Some(c))),
                ),
                out(
                    "0_2.json",
                    "typetree_json",
                    "b",
                    Some((1, Some("SAME"), Some(c))),
                ),
            ],
        );
        let plan = plan_resources(Path::new("/nonexistent"), &[r], &keep_case).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "A/B.assets/gameobject/Same.json",
                "A/B.assets/gameobject/SAME__dup2.json"
            ]
        );
    }

    #[test]
    fn cri_outputs_use_cue_names_and_usm_stems() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("00007")).unwrap();
        std::fs::write(
            dir.path().join("00007/00000.cues.json"),
            br#"[["voice_001",0,0]]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("00007/00001.cues.json"),
            br#"[["voice_002",1,1]]"#,
        )
        .unwrap();
        let acb = res(
            "cri_assets_cri/sound/adv_voice_10436_d0fadf2bf50f57e63be5a3c18c75aa75",
            "00007",
            vec![
                out("00000.wav", "hca_wav", "a", None),
                out("00000.cues.json", "cue_metadata", "b", None),
                out("00001.wav", "hca_wav", "c", None),
                out("00001.cues.json", "cue_metadata", "d", None),
            ],
        );
        let usm = res(
            "cri_assets_cri/video/adv/m_13_3/m_13_3_66782623efcc08538ad7884761168d5d",
            "00008",
            vec![
                out("00000.ivf", "usm_video_stream", "e", None),
                out("00001.wav", "adx_wav", "f", None),
                out("movie.mkv", "usm_mkv", "g", None),
                out("usm.json", "usm_metadata_json", "h", None),
                out("container-mask.json", "usm_masked_json", "i", None),
            ],
        );
        let plan = plan_resources(dir.path(), &[acb, usm], &Options::default()).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "cri/sound/adv_voice_10436/voice_001.wav",
                "cri/sound/adv_voice_10436/voice_001.cues.json",
                "cri/sound/adv_voice_10436/voice_002.wav",
                "cri/sound/adv_voice_10436/voice_002.cues.json",
                "cri/video/adv/m_13_3/m_13_3/m_13_3.ivf",
                "cri/video/adv/m_13_3/m_13_3/m_13_3.wav",
                "cri/video/adv/m_13_3/m_13_3/m_13_3.mkv",
                "cri/video/adv/m_13_3/m_13_3/m_13_3.usm.json",
                "cri/video/adv/m_13_3/m_13_3/m_13_3.container-mask.json",
            ]
        );
        assert_eq!(plan.entries[0].local, "00007/00000.wav");
    }

    #[test]
    fn embedded_acbs_text_asset_groups_and_cueless_tracks_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("00000/0_5-0.acb")).unwrap();
        std::fs::write(
            dir.path().join("00000/0_5-0.acb/00000.cues.json"),
            br#"[["default_flick",0,0]]"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("00001")).unwrap();
        std::fs::write(dir.path().join("00001/00000.cues.json"), b"[]").unwrap();
        std::fs::write(dir.path().join("00001/00001.cues.json"), b"[]").unwrap();
        let se = "Assets/AddressableResources/Cri/Sound/LiveSe/set/default_flick.asset";
        let score = "Assets/AddressableResources/Cri/Sound/MusicScore/a_song.asset";
        let embedded = res(
            "b",
            "00000",
            vec![
                // A MonoBehaviour holding an ACB: its JSON plus the decoded track and cue metadata.
                out(
                    "0_5.json",
                    "typetree_json",
                    "a",
                    Some((114, Some("default_flick"), Some(se))),
                ),
                out(
                    "0_5-0.acb/00000.wav",
                    "hca_wav",
                    "b",
                    Some((114, Some("default_flick"), Some(se))),
                ),
                out(
                    "0_5-0.acb/00000.cues.json",
                    "cue_metadata",
                    "c",
                    Some((114, Some("default_flick"), Some(se))),
                ),
                // Several TextAssets in one container are named individually.
                out(
                    "0_6.bytes",
                    "text_bytes",
                    "d",
                    Some((49, Some("A_Song-002"), Some(score))),
                ),
                out(
                    "0_7.bytes",
                    "text_bytes",
                    "e",
                    Some((49, Some("A_Song-003"), Some(score))),
                ),
            ],
        );
        let cueless = res(
            "cri_assets_cri/sound/bgm_0123456789abcdef0123456789abcdef",
            "00001",
            vec![
                out("00000.wav", "hca_wav", "f", None),
                out("00000.cues.json", "cue_metadata", "g", None),
                out("00001.wav", "hca_wav", "h", None),
                out("00001.cues.json", "cue_metadata", "g", None),
            ],
        );
        let plan = plan_resources(dir.path(), &[embedded, cueless], &Options::default()).unwrap();
        assert_eq!(
            keys(&plan),
            [
                "cri/sound/livese/set/default_flick.json",
                "cri/sound/livese/set/default_flick/default_flick.wav",
                "cri/sound/livese/set/default_flick/default_flick.cues.json",
                "cri/sound/musicscore/a_song.assets/text_asset/a_song-002.bytes",
                "cri/sound/musicscore/a_song.assets/text_asset/a_song-003.bytes",
                "cri/sound/bgm/00000.wav",
                "cri/sound/bgm/00000.cues.json",
                "cri/sound/bgm/00001.wav",
                "cri/sound/bgm/00001.cues.json",
            ]
        );
        assert_eq!(plan.renamed, 0);
    }

    #[test]
    fn names_are_sanitized_and_failed_resources_rejected() {
        let long = "n".repeat(300);
        let r = res(
            "b",
            "00000",
            vec![
                out(
                    "0_1.json",
                    "typetree_json",
                    "a",
                    Some((
                        1,
                        Some("a/b:c(Clone)(Clone)"),
                        Some("Assets/AddressableResources/X.prefab"),
                    )),
                ),
                out(
                    "0_2.json",
                    "typetree_json",
                    "b",
                    Some((1, Some(&long), Some("Assets/AddressableResources/X.prefab"))),
                ),
            ],
        );
        let plan = plan_resources(Path::new("/nonexistent"), &[r], &Options::default()).unwrap();
        assert_eq!(
            plan.entries[0].path,
            "x.assets/gameobject/a_b_c__clone2.json"
        );
        assert!(plan.entries[1].path.ends_with("__truncated.json"));
        let mut failed = res("b", "00000", vec![]);
        failed.errors.push("export failed".into());
        assert!(plan_resources(Path::new("/nonexistent"), &[failed], &Options::default()).is_err());
        assert_eq!(
            snake_case("ParticleSystemRenderer"),
            "particle_system_renderer"
        );
        assert_eq!(snake_case("LODGroup"), "lod_group");
    }
}

#[cfg(test)]
mod real_journal {
    /// Plan a real export directory without publishing:
    /// `SIRIUS_STABLE_JOURNAL_DIR=<export dir> cargo test --lib real_journal -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn plan_real_journal() {
        let dir = std::env::var("SIRIUS_STABLE_JOURNAL_DIR").unwrap();
        let plan = super::plan(std::path::Path::new(&dir), &super::Options::default()).unwrap();
        println!(
            "entries {} deduplicated {} renamed {}",
            plan.entries.len(),
            plan.deduplicated,
            plan.renamed
        );
        let mut tops = std::collections::BTreeMap::new();
        for e in &plan.entries {
            *tops
                .entry(e.path.split('/').next().unwrap_or_default().to_string())
                .or_insert(0usize) += 1;
        }
        println!("top-level directories {tops:?}");
    }
}
