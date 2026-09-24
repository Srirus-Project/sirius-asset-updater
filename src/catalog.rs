//! Bounded Addressables binary v2 location and dependency reader.
use crate::Error;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
const NONE: u32 = u32::MAX;
const MASK: u32 = 0x3fff_ffff;
const DYNAMIC: u32 = 0x4000_0000;
const UNICODE: u32 = 0x8000_0000;
const MAX_LOCATIONS: usize = 100_000;
const MAX_LINKS: usize = 1_000_000;
#[derive(Debug, Serialize)]
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
        let keys = r.array(r.u32(8)?, 8)?;
        let mut pending = Vec::new();
        let mut links = 0;
        for key in keys.as_chunks::<8>().0 {
            let ids = r.ids(u32::from_le_bytes(key[4..8].try_into().unwrap()))?;
            links += ids.len();
            if links > MAX_LINKS {
                return Err(Error::Catalog);
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
        })
    }
}
