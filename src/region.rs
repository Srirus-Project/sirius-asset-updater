//! Region identity is independent of deployment environment and UI language.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// Canonical region identifiers. Serialization always emits one of these names.
pub const NAMES: &[&str] = &["jp", "hk", "en", "kr", "cn"];
/// Pre-1.2.1 name of [`Region::Hk`]. Accepted only as a deprecated input alias.
const LEGACY_HK: &str = "tw";
static LEGACY_HK_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    Jp,
    /// Global Traditional Chinese (TW/HK/MO); the game names it `hk` (`/prod/hk_…`).
    Hk,
    En,
    Kr,
    Cn,
}
impl Region {
    /// Parses a canonical name, or the deprecated `tw` alias for [`Region::Hk`]
    /// (logged once per process). Callers decide whether `cn` is acceptable.
    pub fn from_name(value: &str) -> Option<Self> {
        Some(match value {
            "jp" => Self::Jp,
            "hk" => Self::Hk,
            "en" => Self::En,
            "kr" => Self::Kr,
            "cn" => Self::Cn,
            LEGACY_HK => {
                if !LEGACY_HK_WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        region = "hk",
                        "Deprecated region alias accepted as hk; update inputs to use hk"
                    );
                }
                Self::Hk
            }
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::Hk => "hk",
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
            Self::Hk => Some("2"),
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
            Some(Self::Hk)
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
    /// Frozen hash input for identities persisted before 1.2.1 (completion-target digests),
    /// so pending deliveries survive the rename. Never emitted as a region name.
    pub(crate) fn persisted_digest_tag(self) -> &'static str {
        if self == Self::Hk {
            LEGACY_HK
        } else {
            self.name()
        }
    }
    pub fn default_platform(self) -> Platform {
        if self == Self::Jp {
            Platform::Ios
        } else {
            Platform::Android
        }
    }
}
impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Region;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a region name")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Region, E> {
                Region::from_name(value).ok_or_else(|| E::unknown_variant(value, NAMES))
            }
        }
        deserializer.deserialize_str(Visitor)
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
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tracing_subscriber::layer::SubscriberExt;

    /// Serializes tests that parse the legacy alias so the once-only warning is observable.
    pub(crate) static LEGACY_ALIAS_LOCK: Mutex<()> = Mutex::new(());

    struct CountWarnings(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountWarnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Runs `f` with the once-only flag reset and returns the number of warnings it logged.
    pub(crate) fn legacy_warnings(f: impl FnOnce()) -> usize {
        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarnings(count.clone()));
        LEGACY_HK_WARNED.store(false, Ordering::SeqCst);
        tracing::subscriber::with_default(subscriber, f);
        count.load(Ordering::SeqCst)
    }

    #[test]
    fn hk_is_canonical_and_tw_is_a_deprecated_input_alias() {
        let _lock = LEGACY_ALIAS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(Region::Hk.name(), "hk");
        assert_eq!(sonic_rs::to_string(&Region::Hk).unwrap(), r#""hk""#);
        assert!(!NAMES.contains(&LEGACY_HK));
        assert_eq!(
            legacy_warnings(|| {
                assert_eq!(sonic_rs::from_str::<Region>(r#""hk""#).unwrap(), Region::Hk);
                assert_eq!(Region::from_name("hk"), Some(Region::Hk));
            }),
            0
        );
        assert_eq!(
            legacy_warnings(|| {
                let json: Region = sonic_rs::from_str(r#""tw""#).unwrap();
                let yaml: Region = yaml_serde::from_str("tw").unwrap();
                assert_eq!((json, yaml), (Region::Hk, Region::Hk));
                assert_eq!(Region::from_name("tw"), Some(Region::Hk));
                // Round trips never re-emit the alias.
                assert_eq!(sonic_rs::to_string(&json).unwrap(), r#""hk""#);
            }),
            1
        );
        for bad in [r#""TW""#, r#""HK""#, r#""global""#, r#""""#, "2"] {
            assert!(sonic_rs::from_str::<Region>(bad).is_err(), "{bad}");
        }
        assert!(Region::Hk.matches_known_service("l12-prod-hk-a.gamerfusiontech.com", "/"));
        assert!(!Region::En.matches_known_service("l12-prod-hk-a.gamerfusiontech.com", "/"));
        assert_eq!(Region::Hk.area_id(), Some("2"));
    }
}
