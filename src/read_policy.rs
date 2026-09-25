//! Explicit Unity object representation, separate from object selection.
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Auto,
    ObjectRaw,
    TypetreeJson,
    Image,
    ImageArchive,
    Audio,
    Video,
    TextBytes,
    Font,
    Shader,
    Obj,
}
impl Kind {
    pub fn supports(self, class: i32) -> bool {
        match self {
            Self::Auto | Self::ObjectRaw | Self::TypetreeJson => class > 0,
            Self::Image => matches!(
                class,
                28 | 213 | unity_rs_core::texture_array::TEXTURE_2D_ARRAY_CLASS_ID
            ),
            Self::ImageArchive => class == unity_rs_core::texture_array::TEXTURE_2D_ARRAY_CLASS_ID,
            Self::Audio => class == unity_rs_core::simple_assets::AUDIO_CLIP_CLASS_ID,
            Self::Video => matches!(
                class,
                unity_rs_core::simple_assets::VIDEO_CLIP_CLASS_ID
                    | unity_rs_core::simple_assets::MOVIE_TEXTURE_CLASS_ID
            ),
            Self::TextBytes => class == 49,
            Self::Font => class == 128,
            Self::Shader => class == 48,
            Self::Obj => class == 43,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub default: Kind,
    pub classes: BTreeMap<i32, Kind>,
}
impl Policy {
    pub fn validate(&self) -> Result<(), Error> {
        if self.classes.len() > 256
            || self
                .classes
                .iter()
                .any(|(&class, &kind)| !kind.supports(class))
        {
            return Err(Error::Config);
        }
        Ok(())
    }
    pub fn for_class(&self, class: i32) -> Result<Kind, Error> {
        let kind = self.classes.get(&class).copied().unwrap_or(self.default);
        if kind.supports(class) {
            Ok(kind)
        } else {
            Err(Error::Config)
        }
    }
    /// Nondefault representations are never advertised as the complete native decoded export.
    pub fn is_native(&self) -> bool {
        self.default == Kind::Auto && self.classes.values().all(|kind| *kind == Kind::Auto)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn class_overrides_precede_default_and_incompatible_modes_fail() {
        let policy: Policy =
            yaml_serde::from_str("default: object_raw\nclasses: {28: image, 114: typetree_json}")
                .unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.for_class(28).unwrap(), Kind::Image);
        assert_eq!(policy.for_class(114).unwrap(), Kind::TypetreeJson);
        assert_eq!(policy.for_class(49).unwrap(), Kind::ObjectRaw);
        assert!(!policy.is_native());
        assert!(Policy::default().is_native());
        for yaml in [
            "classes: {114: image}",
            "classes: {0: object_raw}",
            "classes: {28: font}",
        ] {
            let invalid: Policy = yaml_serde::from_str(yaml).unwrap();
            assert!(invalid.validate().is_err());
        }
        for yaml in [
            "default: ignored",
            "classes: {Texture2D: image}",
            "unknown: true",
        ] {
            assert!(yaml_serde::from_str::<Policy>(yaml).is_err());
        }
        let image_default: Policy = yaml_serde::from_str("default: image").unwrap();
        assert!(image_default.for_class(28).is_ok());
        assert!(image_default.for_class(114).is_err());
    }
}
