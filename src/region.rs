//! Region identity is independent of deployment environment and UI language.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    Jp,
    Tw,
    En,
    Kr,
    Cn,
}
impl Region {
    pub fn name(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::Tw => "tw",
            Self::En => "en",
            Self::Kr => "kr",
            Self::Cn => "cn",
        }
    }
    pub fn family(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::Cn => "cn",
            _ => "global",
        }
    }
    pub fn area_id(self) -> Option<&'static str> {
        match self {
            Self::Tw => Some("2"),
            Self::En => Some("3"),
            Self::Kr => Some("4"),
            _ => None,
        }
    }
    pub fn protocol_version(self) -> &'static str {
        match self {
            Self::Jp => "1.0.3",
            Self::Cn => "",
            _ => "1.0.1",
        }
    }
    pub fn matches_known_service(self, host: &str, path: &str) -> bool {
        let expected = if host.ends_with(".bang-dream-on.jp") {
            Some(Self::Jp)
        } else if host.ends_with(".gamerfusiontech.com") && host.contains("-prod-hk-") {
            Some(Self::Tw)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-va-") {
            Some(Self::En)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-kr-") {
            Some(Self::Kr)
        } else if host.ends_with(".bilibiligame.net") && host.contains("-prod-sg-patch-") {
            if path.starts_with("/prod/en_") {
                Some(Self::En)
            } else if path.starts_with("/prod/kr_") {
                Some(Self::Kr)
            } else {
                return false;
            }
        } else {
            None
        };
        expected.is_none_or(|region| region == self)
    }
    pub fn default_platform(self) -> Platform {
        if self == Self::Jp {
            Platform::Ios
        } else {
            Platform::Android
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Platform {
    #[serde(rename = "iOS")]
    Ios,
    Android,
}
impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::Android => "Android",
        }
    }
    pub fn header(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
        }
    }
}
