//! Encoding backend policy; stream-copy muxing and independent verification remain CLI operations.
use crate::Error;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    #[default]
    Cli,
    Ffi,
    Auto,
}
impl Backend {
    pub fn validate(self) -> Result<(), Error> {
        if self == Self::Ffi {
            #[cfg(not(feature = "media-ffi"))]
            return Err(Error::Config);
            #[cfg(feature = "media-ffi")]
            crate::media_ffi::runtime_identity().map_err(|_| Error::Config)?;
        }
        Ok(())
    }
    pub(crate) fn identity(self) -> String {
        if self == Self::Cli {
            return "cli".into();
        }
        #[cfg(feature = "media-ffi")]
        {
            crate::media_ffi::runtime_identity().unwrap_or_else(|_| "ffi-unavailable".into())
        }
        #[cfg(not(feature = "media-ffi"))]
        {
            "cli-only-build".into()
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Encoding {
    Flac,
    Mp3,
    Mp4,
}
