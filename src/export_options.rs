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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PngCompression {
    #[default]
    Fast,
    Default,
    Best,
}
impl PngCompression {
    fn is_fast(&self) -> bool {
        *self == Self::Fast
    }
    fn native(self) -> unity_rs_core::image_export::PngCompression {
        use unity_rs_core::image_export::PngCompression as P;
        match self {
            Self::Fast => P::Fast,
            Self::Default => P::Default,
            Self::Best => P::Best,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageExport {
    Png {
        #[serde(default, skip_serializing_if = "PngCompression::is_fast")]
        compression: PngCompression,
    },
    Webp {},
    Bmp {},
    Tga {},
    Jpeg {
        quality: u8,
        background: [u8; 3],
    },
}
impl Default for ImageExport {
    fn default() -> Self {
        Self::Png {
            compression: PngCompression::Fast,
        }
    }
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
            Self::Png { .. } => F::Png,
            Self::Webp {} => F::Webp,
            Self::Bmp {} => F::Bmp,
            Self::Tga {} => F::Tga,
            Self::Jpeg { .. } => F::Jpeg,
        }
    }
    pub fn encode(
        &self,
        image: &unity_rs_core::texture::RgbaImage,
        order: unity_rs_core::image_export::ImageRowOrder,
        limit: u64,
    ) -> Result<Vec<u8>, Error> {
        use unity_rs_core::image_export::{write_rgba_image_with_options, ImageEncodeOptions};
        self.validate()?;
        let mut options = ImageEncodeOptions {
            png_compression: match self {
                Self::Png { compression } => compression.native(),
                _ => PngCompression::Fast.native(),
            },
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
/// Canonical, nonempty rendition set; single objects preserve the legacy format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFormats(Vec<ImageExport>);
impl Default for ImageFormats {
    fn default() -> Self {
        ImageExport::default().into()
    }
}
impl From<ImageExport> for ImageFormats {
    fn from(value: ImageExport) -> Self {
        Self(vec![value])
    }
}
impl ImageFormats {
    pub fn iter(&self) -> impl Iterator<Item = &ImageExport> {
        self.0.iter()
    }
    pub fn validate(&self) -> Result<(), Error> {
        for format in &self.0 {
            format.validate()?;
        }
        Ok(())
    }
}
impl Serialize for ImageFormats {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.len() == 1 {
            self.0[0].serialize(serializer)
        } else {
            self.0.serialize(serializer)
        }
    }
}
impl<'de> Deserialize<'de> for ImageFormats {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            One(ImageExport),
            Many(Vec<ImageExport>),
        }
        let mut values = match Input::deserialize(deserializer)? {
            Input::One(value) => vec![value],
            Input::Many(values) => values,
        };
        if values.is_empty() || values.len() > 5 {
            return Err(serde::de::Error::custom("select one to five image formats"));
        }
        values.sort_by_key(|format| format.native().extension());
        if values
            .windows(2)
            .any(|pair| pair[0].native() == pair[1].native())
        {
            return Err(serde::de::Error::custom("duplicate image format"));
        }
        let formats = Self(values);
        formats
            .validate()
            .map_err(|_| serde::de::Error::custom("invalid image format options"))?;
        Ok(formats)
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

#[cfg(test)]
mod png_tests {
    use super::*;
    use unity_rs_core::{image_export::ImageRowOrder, texture::RgbaImage};
    #[test]
    fn image_sets_are_canonical_bounded_and_reject_colliding_extensions() {
        let single: ImageFormats = yaml_serde::from_str("{format: png}").unwrap();
        let list: ImageFormats =
            yaml_serde::from_str("[{format: png, compression: fast}]").unwrap();
        assert_eq!(single, list);
        assert_eq!(sonic_rs::to_string(&single).unwrap(), r#"{"format":"png"}"#);
        let a: ImageFormats = yaml_serde::from_str("[{format: webp}, {format: png}]").unwrap();
        let b: ImageFormats = yaml_serde::from_str("[{format: png}, {format: webp}]").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            sonic_rs::to_string(&a).unwrap(),
            sonic_rs::to_string(&b).unwrap()
        );
        for invalid in ["[]", "[{format: png}, {format: png, compression: best}]",
            "[{format: jpeg, quality: 90, background: [0,0,0]}, {format: jpeg, quality: 100, background: [255,255,255]}]",
            "[{format: jpeg, quality: 0, background: [0,0,0]}]", "[{format: webp, compression: best}]", "[null]"] {
            assert!(yaml_serde::from_str::<ImageFormats>(invalid).is_err(),"accepted {invalid}");
        }
    }
    #[test]
    fn compression_changes_encoding_preserves_rgba_and_legacy_config() {
        let legacy: ImageExport = yaml_serde::from_str("format: png").unwrap();
        assert_eq!(sonic_rs::to_string(&legacy).unwrap(), r#"{"format":"png"}"#);
        let pixels: Vec<u8> = (0..64)
            .flat_map(|y| {
                (0..128).flat_map(move |x| {
                    [
                        (x * 2) as u8,
                        (y * 3) as u8,
                        ((x / 8 + y / 8) * 13) as u8,
                        ((x + y) % 256) as u8,
                    ]
                })
            })
            .collect();
        let image = RgbaImage {
            width: 128,
            height: 64,
            pixels: pixels.clone(),
        };
        let baseline = legacy
            .encode(&image, ImageRowOrder::Display, 1024 * 1024)
            .unwrap();
        let mut encodings = std::collections::HashSet::new();
        for compression in ["fast", "default", "best"] {
            let config: ImageExport =
                yaml_serde::from_str(&format!("format: png\ncompression: {compression}")).unwrap();
            for (flipped, order) in [
                (false, ImageRowOrder::Display),
                (true, ImageRowOrder::UnityDecoded),
            ] {
                let bytes = config.encode(&image, order, 1024 * 1024).unwrap();
                if !flipped {
                    if compression == "fast" {
                        assert_eq!(bytes, baseline);
                    }
                    encodings.insert(bytes.clone());
                }
                let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes))
                    .read_info()
                    .unwrap();
                let mut decoded = vec![0; decoder.output_buffer_size().unwrap()];
                let info = decoder.next_frame(&mut decoded).unwrap();
                assert_eq!(
                    (info.width, info.height, info.color_type),
                    (128, 64, png::ColorType::Rgba)
                );
                let expected: Vec<u8> = if flipped {
                    pixels
                        .chunks_exact(128 * 4)
                        .rev()
                        .flatten()
                        .copied()
                        .collect()
                } else {
                    pixels.clone()
                };
                assert_eq!(&decoded[..info.buffer_size()], expected);
                assert!(config.encode(&image, order, 1).is_err());
            }
            let json = sonic_rs::to_string(&config).unwrap();
            let restored: ImageExport = sonic_rs::from_str(&json).unwrap();
            assert_eq!(
                restored
                    .encode(&image, ImageRowOrder::Display, 1024 * 1024)
                    .unwrap(),
                config
                    .encode(&image, ImageRowOrder::Display, 1024 * 1024)
                    .unwrap()
            );
        }
        assert!(
            encodings.len() > 1,
            "compression must affect actual encoded bytes"
        );
        for invalid in [
            "format: png\ncompression: 9",
            "format: png\ncompression: unknown",
            "format: webp\ncompression: best",
            "format: bmp\nquality: 90",
            "format: tga\ncompression: fast",
        ] {
            assert!(
                yaml_serde::from_str::<ImageExport>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}

/// Container retention is independent of final decoded audio/video formats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerMode {
    #[default]
    Decode,
    Preserve,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CriExport {
    pub acb: ContainerMode,
    pub usm: ContainerMode,
}
impl CriExport {
    pub fn full(&self) -> bool {
        self.acb == ContainerMode::Decode && self.usm == ContainerMode::Decode
    }
}

/// Bounded retry of individual FFmpeg child processes after a failure classified as transient.
/// Scheduling only: it never changes an accepted output, so it is excluded from cache identity.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaRetry {
    pub attempts: usize,
    pub delay_ms: u64,
    pub max_delay_ms: u64,
}
impl Default for MediaRetry {
    fn default() -> Self {
        Self {
            attempts: 1,
            delay_ms: 1000,
            max_delay_ms: 4000,
        }
    }
}
impl MediaRetry {
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=8).contains(&self.attempts)
            || self.delay_ms > 60_000
            || self.max_delay_ms > 60_000
            || self.delay_ms > self.max_delay_ms
        {
            return Err(Error::Config);
        }
        Ok(())
    }
    /// Delay before retry number `retry` (0-based): doubling, capped, without jitter.
    pub fn delay(&self, retry: usize) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.delay_ms
                .saturating_mul(1u64.checked_shl(retry as u32).unwrap_or(u64::MAX))
                .min(self.max_delay_ms),
        )
    }
}
