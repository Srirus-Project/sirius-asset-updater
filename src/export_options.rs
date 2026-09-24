use crate::{assets::Provider, Error};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportProvider {
    Unity,
    Cri,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selection {
    pub providers: Vec<ExportProvider>,
    pub unity_class_ids: Vec<i32>,
    pub embedded_audio: bool,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            providers: vec![],
            unity_class_ids: vec![],
            embedded_audio: true,
        }
    }
}
impl Selection {
    pub fn validate(&self) -> Result<(), Error> {
        if self.providers.len() > 2
            || (self.providers.len() == 2 && self.providers[0] == self.providers[1])
            || self.unity_class_ids.len() > 256
            || self.unity_class_ids.iter().any(|id| *id <= 0)
            || self
                .unity_class_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.unity_class_ids.len()
            || (!self.unity_class_ids.is_empty() && !self.provider(Provider::UnityBundle))
        {
            return Err(Error::Config);
        }
        Ok(())
    }
    pub fn provider(&self, provider: Provider) -> bool {
        self.providers.is_empty()
            || self.providers.contains(&match provider {
                Provider::Cri => ExportProvider::Cri,
                _ => ExportProvider::Unity,
            })
    }
    pub fn class(&self, id: i32) -> bool {
        self.unity_class_ids.is_empty() || self.unity_class_ids.contains(&id)
    }
    pub fn full(&self) -> bool {
        self.provider(Provider::Cri)
            && self.provider(Provider::UnityBundle)
            && self.unity_class_ids.is_empty()
            && self.embedded_audio
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageExport {
    #[default]
    Png,
    Webp,
    Bmp,
    Tga,
    Jpeg {
        quality: u8,
        background: [u8; 3],
    },
}
impl ImageExport {
    pub fn validate(&self) -> Result<(), Error> {
        if matches!(self, Self::Jpeg { quality, .. } if !(1..=100).contains(quality)) {
            return Err(Error::Config);
        }
        Ok(())
    }
    pub fn native(&self) -> unity_rs_core::image_export::ImageFormat {
        use unity_rs_core::image_export::ImageFormat as F;
        match self {
            Self::Png => F::Png,
            Self::Webp => F::Webp,
            Self::Bmp => F::Bmp,
            Self::Tga => F::Tga,
            Self::Jpeg { .. } => F::Jpeg,
        }
    }
    pub fn encode(
        &self,
        image: &unity_rs_core::texture::RgbaImage,
        order: unity_rs_core::image_export::ImageRowOrder,
        limit: u64,
    ) -> Result<Vec<u8>, Error> {
        use unity_rs_core::image_export::{
            write_rgba_image_with_options, ImageEncodeOptions, PngCompression,
        };
        self.validate()?;
        let mut options = ImageEncodeOptions {
            png_compression: PngCompression::Fast,
            maximum_output_bytes: limit,
            ..Default::default()
        };
        if let Self::Jpeg {
            quality,
            background,
        } = self
        {
            options.jpeg_quality = *quality;
            options.jpeg_background = Some(*background);
        }
        let mut bytes = vec![];
        write_rgba_image_with_options(image, self.native(), order, &options, &mut bytes)
            .map_err(|e| Error::Export(e.to_string()))?;
        Ok(bytes)
    }
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AudioExport {
    #[default]
    Wav,
    Flac,
    Mp3,
}

/// Native elementary streams are always preserved with their actual codec extension.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoExport {
    Source,
    #[default]
    Mkv,
    Mp4,
    MkvAndMp4,
}

/// Canonical nonempty format set, accepting the legacy scalar YAML spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormats(Vec<AudioExport>);
impl Default for AudioFormats {
    fn default() -> Self {
        AudioExport::Wav.into()
    }
}
impl From<AudioExport> for AudioFormats {
    fn from(value: AudioExport) -> Self {
        Self(vec![value])
    }
}
impl AudioFormats {
    pub fn contains(&self, value: AudioExport) -> bool {
        self.0.contains(&value)
    }
}
impl Serialize for AudioFormats {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.len() == 1 {
            self.0[0].serialize(serializer)
        } else {
            self.0.serialize(serializer)
        }
    }
}
impl<'de> Deserialize<'de> for AudioFormats {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            One(AudioExport),
            Many(Vec<AudioExport>),
        }
        let mut values = match Input::deserialize(deserializer)? {
            Input::One(value) => vec![value],
            Input::Many(values) => values,
        };
        if values.is_empty() || values.len() > 3 {
            return Err(serde::de::Error::custom(
                "select one to three audio formats",
            ));
        }
        values.sort();
        if values.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(serde::de::Error::custom("duplicate audio format"));
        }
        Ok(Self(values))
    }
}
