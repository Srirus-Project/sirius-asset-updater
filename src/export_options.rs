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
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioExport {
    #[default]
    Wav,
    Flac,
}
