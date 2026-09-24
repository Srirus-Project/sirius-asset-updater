//! Bounded Addressables binary v2 location and dependency reader.
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
const NONE: u32 = u32::MAX;
const MASK: u32 = 0x3fff_ffff;
const DYNAMIC: u32 = 0x4000_0000;
const UNICODE: u32 = 0x8000_0000;
const MAX_LOCATIONS: usize = 100_000;
const MAX_LINKS: usize = 1_000_000;
#[derive(Clone, Debug, Serialize)]
pub struct Location {
    pub id: u32,
    pub primary_key: String,
    pub internal_id: String,
    pub provider_id: String,
    pub dependencies: Vec<u32>,
}
#[derive(Debug, Serialize)]
pub struct Catalog {
    pub locations: Vec<Location>,
    // Preserve the v1 location-graph serialization for existing receipts.
    #[serde(skip)]
    pub keys: BTreeMap<String, Vec<u32>>,
}
/// Native Addressables string keys (labels or explicit asset addresses).
/// Empty selects the entire catalog; named keys select their union and dependencies.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub priority: Vec<String>,
}
fn patterns(values: &[String]) -> Result<Vec<regex::Regex>, Error> {
    if values.len() > 128 || values.iter().any(|v| v.len() > 2048) {
        return Err(Error::Config);
    }
    values
        .iter()
        .map(|v| {
            regex::RegexBuilder::new(v)
                .size_limit(1024 * 1024)
                .build()
                .map_err(|_| Error::Config)
        })
        .collect()
}
impl Selection {
    pub fn prioritize(&self, plan: &mut crate::assets::Plan) -> Result<(), Error> {
        let patterns = patterns(&self.priority)?;
        plan.assets.sort_by_cached_key(|a| {
            (
                patterns
                    .iter()
                    .position(|p| p.is_match(&a.relative_path))
                    .unwrap_or(patterns.len()),
                a.relative_path.clone(),
            )
        });
        Ok(())
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self.keys.len() > 256
            || self
                .keys
                .iter()
                .any(|k| k.is_empty() || k.len() > 2048 || k.chars().any(char::is_control))
        {
            return Err(Error::Config);
        }
        patterns(&self.include)?;
        patterns(&self.exclude)?;
        patterns(&self.priority)?;
        Ok(())
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    string_budget: usize,
}
impl Reader<'_> {
    fn bytes(&self, offset: usize, len: usize) -> Result<&[u8], Error> {
        self.bytes
            .get(offset..offset.checked_add(len).ok_or(Error::Catalog)?)
            .ok_or(Error::Catalog)
    }
    fn u32(&self, offset: usize) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.bytes(offset, 4)?
                .try_into()
                .map_err(|_| Error::Catalog)?,
        ))
    }
    fn array(&self, id: u32, stride: usize) -> Result<&[u8], Error> {
        if id == NONE {
            return Ok(&[]);
        }
        let offset = id as usize;
        let size = self.u32(offset.checked_sub(4).ok_or(Error::Catalog)?)? as usize;
        if !size.is_multiple_of(stride) || size / stride > MAX_LINKS {
            return Err(Error::Catalog);
        }
        self.bytes(offset, size)
    }
    fn plain_string(&mut self, id: u32) -> Result<String, Error> {
        if id == NONE {
            return Ok(String::new());
        }
        let offset = (id & MASK) as usize;
        let len = self.u32(offset.checked_sub(4).ok_or(Error::Catalog)?)? as usize;
        if len > 64 * 1024 {
            return Err(Error::Catalog);
        }
        self.string_budget = self.string_budget.checked_sub(len).ok_or(Error::Catalog)?;
        let bytes = self.bytes(offset, len)?;
        if id & UNICODE != 0 {
            if !len.is_multiple_of(2) {
                return Err(Error::Catalog);
            }
            let units: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            String::from_utf16(&units).map_err(|_| Error::Catalog)
        } else {
            if !bytes.is_ascii() {
                return Err(Error::Catalog);
            }
            String::from_utf8(bytes.to_vec()).map_err(|_| Error::Catalog)
        }
    }
    fn string(&mut self, id: u32, separator: &str) -> Result<String, Error> {
        if id == NONE || id & DYNAMIC == 0 {
            return self.plain_string(id);
        }
        let mut seen = BTreeSet::new();
        let mut parts = Vec::new();
        let mut next = id;
        let mut len = 0;
        while next != NONE {
            let offset = next & MASK;
            if !seen.insert(offset) || seen.len() > 256 {
                return Err(Error::Catalog);
            }
            let part = self.plain_string(self.u32(offset as usize)?)?;
            len += part.len() + separator.len();
            if len > 64 * 1024 {
                return Err(Error::Catalog);
            }
            parts.push(part);
            next = self.u32(offset as usize + 4)?;
        }
        parts.reverse();
        Ok(parts.join(separator))
    }
    fn ids(&self, id: u32) -> Result<Vec<u32>, Error> {
        Ok(self
            .array(id, 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect())
    }
}
impl Catalog {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(Error::Size);
        }
        let mut r = Reader {
            bytes,
            string_budget: 64 * 1024 * 1024,
        };
        if r.u32(0)? != 0x0de38942 || r.u32(4)? != 2 {
            return Err(Error::Catalog);
        }
        let keys = r.array(r.u32(8)?, 8)?.to_vec();
        let mut key_index = BTreeMap::<String, Vec<u32>>::new();
        let mut pending = Vec::new();
        let mut links = 0;
        for key in keys.as_chunks::<8>().0 {
            let ids = r.ids(u32::from_le_bytes(key[4..8].try_into().unwrap()))?;
            links += ids.len();
            if links > MAX_LINKS {
                return Err(Error::Catalog);
            }
            let object = u32::from_le_bytes(key[..4].try_into().unwrap());
            if object != NONE {
                // Serialized object: [type descriptor, data]. Type: [assembly, full name].
                let ty = r.u32(object as usize)?;
                let type_name = r.string(r.u32(ty as usize + 4)?, ".")?;
                if type_name == "System.String" {
                    let data = r.u32(object as usize + 4)?;
                    let name = r.string(r.u32(data as usize)?, "/")?;
                    if name.is_empty() {
                        return Err(Error::Catalog);
                    }
                    key_index
                        .entry(name)
                        .or_default()
                        .extend(ids.iter().copied());
                }
            }
            pending.extend(ids);
        }
        let mut locations = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if locations.contains_key(&id) {
                continue;
            }
            if locations.len() >= MAX_LOCATIONS {
                return Err(Error::Catalog);
            }
            let offset = id as usize;
            r.bytes(offset, 24)?;
            let dependencies = r.ids(r.u32(offset + 12)?)?;
            links += dependencies.len();
            if links > MAX_LINKS {
                return Err(Error::Catalog);
            }
            pending.extend(dependencies.iter().copied());
            let location = Location {
                id,
                primary_key: r.string(r.u32(offset)?, "/")?,
                internal_id: r.string(r.u32(offset + 4)?, "/")?,
                provider_id: r.string(r.u32(offset + 8)?, ".")?,
                dependencies,
            };
            locations.insert(id, location);
        }
        if locations.is_empty() {
            return Err(Error::Catalog);
        }
        Ok(Self {
            locations: locations.into_values().collect(),
            keys: key_index,
        })
    }
}

impl Catalog {
    pub fn select(&self, selection: &Selection) -> Result<Self, Error> {
        selection.validate()?;
        let by_id: BTreeMap<_, _> = self.locations.iter().map(|l| (l.id, l)).collect();
        let mut roots = Vec::new();
        if selection.keys.is_empty() {
            roots.extend(self.locations.iter().map(|l| l.id));
        } else {
            for key in &selection.keys {
                let ids = self.keys.get(key).ok_or(Error::Selection)?;
                if ids.is_empty() {
                    return Err(Error::Selection);
                }
                roots.extend(ids.iter().copied());
            }
        }
        let include = patterns(&selection.include)?;
        let exclude = patterns(&selection.exclude)?;
        let mut pending = Vec::new();
        for id in roots {
            let l = by_id.get(&id).ok_or(Error::Catalog)?;
            let matches =
                |p: &regex::Regex| p.is_match(&l.primary_key) || p.is_match(&l.internal_id);
            if (include.is_empty() || include.iter().any(matches)) && !exclude.iter().any(matches) {
                pending.push(id);
            }
        }
        if pending.is_empty() {
            return Err(Error::Selection);
        }
        let mut selected = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !selected.insert(id) {
                continue;
            }
            let location = by_id.get(&id).ok_or(Error::Catalog)?;
            pending.extend(location.dependencies.iter().copied());
        }
        Ok(Self {
            locations: self
                .locations
                .iter()
                .filter(|l| selected.contains(&l.id))
                .cloned()
                .collect(),
            keys: BTreeMap::new(),
        })
    }
}
