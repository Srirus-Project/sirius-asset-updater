//! Independent per-export decoder admission; execution remains cooperative.
use crate::{
    media_gate::{Gate, Permit},
    Error,
};
use serde::Deserialize;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto_tune: bool,
    pub acb: Option<usize>,
    pub usm: Option<usize>,
    pub hca: Option<usize>,
    pub image: Option<usize>,
    pub audio_encode: Option<usize>,
    pub video_encode: Option<usize>,
    pub wait_timeout_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_tune: false,
            acb: None,
            usm: None,
            hca: None,
            image: None,
            audio_encode: None,
            video_encode: None,
            wait_timeout_seconds: 3600,
        }
    }
}
impl Config {
    /// Automatic widths are budget-derived, not corpus-specific throughput predictions.
    /// Explicit values remain upper bounds, including on hosts larger than the original one.
    pub fn effective(&self, cpu: &crate::cpu_policy::Config, cpus: usize) -> Result<Self, Error> {
        self.validate()?;
        let budget = cpu.budget_for_cpus(cpus)?;
        if !self.auto_tune {
            return Ok(self.clone());
        }
        let single = budget.clamp(1, 64);
        // Both current CLI and FFI MP4 encoders use two video encoder threads.
        // Other decoder/codec threads remain outside this admission estimate.
        let video = (budget / 2).clamp(1, 64);
        let limit = |configured: Option<usize>, automatic: usize| {
            Some(configured.unwrap_or(64).min(automatic))
        };
        Ok(Self {
            auto_tune: true,
            acb: limit(self.acb, single),
            usm: limit(self.usm, single),
            hca: limit(self.hca, single),
            image: limit(self.image, single),
            audio_encode: limit(self.audio_encode, single),
            video_encode: limit(self.video_encode, video),
            wait_timeout_seconds: self.wait_timeout_seconds,
        })
    }

    pub fn validate(&self) -> Result<(), Error> {
        if [
            self.acb,
            self.usm,
            self.hca,
            self.image,
            self.audio_encode,
            self.video_encode,
        ]
        .into_iter()
        .flatten()
        .any(|n| !(1..=64).contains(&n))
            || !(1..=3600).contains(&self.wait_timeout_seconds)
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Acb,
    Usm,
    Hca,
    Image,
    AudioEncode,
    VideoEncode,
}
#[derive(Default)]
pub(crate) struct Gates {
    acb: Gate,
    usm: Gate,
    hca: Gate,
    image: Gate,
    audio_encode: Gate,
    video_encode: Gate,
}
impl Gates {
    pub(crate) fn acquire(
        &self,
        config: &Config,
        stage: Stage,
        cancel: &AtomicBool,
    ) -> Result<Option<Permit<'_>>, Error> {
        self.acquire_until(
            config,
            stage,
            cancel,
            Instant::now() + Duration::from_secs(config.wait_timeout_seconds),
        )
    }
    pub(crate) fn acquire_until(
        &self,
        config: &Config,
        stage: Stage,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Option<Permit<'_>>, Error> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let (gate, limit) = match stage {
            Stage::Acb => (&self.acb, config.acb),
            Stage::Usm => (&self.usm, config.usm),
            Stage::Hca => (&self.hca, config.hca),
            Stage::Image => (&self.image, config.image),
            Stage::AudioEncode => (&self.audio_encode, config.audio_encode),
            Stage::VideoEncode => (&self.video_encode, config.video_encode),
        };
        limit
            .map(|limit| {
                gate.acquire(
                    limit,
                    cancel,
                    deadline.min(Instant::now() + Duration::from_secs(config.wait_timeout_seconds)),
                )
                .map_err(|error| match error {
                    Error::Export(_) => Error::Export("decoder stage admission timed out".into()),
                    other => other,
                })
            })
            .transpose()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_widths_use_budget_keep_explicit_caps_and_preserve_manual_defaults() {
        let cpu = crate::cpu_policy::Config {
            budget_ratio: 0.5,
            reserved: 2,
            ..Default::default()
        };
        let manual = Config::default().effective(&cpu, 64).unwrap();
        assert!(manual.hca.is_none() && manual.video_encode.is_none());
        let config = Config {
            auto_tune: true,
            image: Some(3),
            ..Default::default()
        };
        let effective = config.effective(&cpu, 64).unwrap();
        assert_eq!(effective.hca, Some(30));
        assert_eq!(effective.acb, Some(30));
        assert_eq!(effective.usm, Some(30));
        assert_eq!(effective.audio_encode, Some(30));
        assert_eq!(effective.video_encode, Some(15));
        assert_eq!(effective.image, Some(3));
        for cpus in [0, 1, 2] {
            let narrow = config.effective(&cpu, cpus).unwrap();
            assert_eq!(narrow.hca, Some(1));
            assert_eq!(narrow.video_encode, Some(1));
        }
        let cpu = crate::cpu_policy::Config {
            budget_auto: false,
            ..cpu
        };
        let huge = config.effective(&cpu, usize::MAX).unwrap();
        assert_eq!(huge.hca, Some(64));
        assert_eq!(huge.video_encode, Some(64));
        let cfg: Config = yaml_serde::from_str("auto_tune: true\nvideo_encode: 1").unwrap();
        assert_eq!(cfg.effective(&cpu, 64).unwrap().video_encode, Some(1));
    }
    #[test]
    fn independent_stages_validate_and_recover_after_timeout() {
        let cfg = Config {
            acb: Some(1),
            image: Some(1),
            wait_timeout_seconds: 1,
            ..Config::default()
        };
        let gates = Gates::default();
        let cancel = AtomicBool::new(false);
        let held = gates.acquire(&cfg, Stage::Image, &cancel).unwrap();
        assert!(gates.acquire(&cfg, Stage::Acb, &cancel).unwrap().is_some());
        assert!(matches!(
            gates.acquire(&cfg, Stage::Image, &cancel),
            Err(Error::Export(_))
        ));
        drop(held);
        assert!(gates.acquire(&cfg, Stage::Image, &cancel).is_ok());
        for field in ["acb", "usm", "hca", "image", "audio_encode", "video_encode"] {
            for value in [0, 65] {
                let cfg: Config = yaml_serde::from_str(&format!("{field}: {value}")).unwrap();
                assert!(cfg.validate().is_err());
            }
        }
        assert!(yaml_serde::from_str::<Config>("unknown: 1").is_err());
        for seconds in [0, 3601] {
            assert!(Config {
                wait_timeout_seconds: seconds,
                ..Config::default()
            }
            .validate()
            .is_err());
        }
    }
}
