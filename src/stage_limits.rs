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
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub acb: Option<usize>,
    pub usm: Option<usize>,
    pub hca: Option<usize>,
    pub image: Option<usize>,
    pub wait_timeout_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            acb: None,
            usm: None,
            hca: None,
            image: None,
            wait_timeout_seconds: 3600,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if [self.acb, self.usm, self.hca, self.image]
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
}
#[derive(Default)]
pub(crate) struct Gates {
    acb: Gate,
    usm: Gate,
    hca: Gate,
    image: Gate,
}
impl Gates {
    pub(crate) fn acquire(
        &self,
        config: &Config,
        stage: Stage,
        cancel: &AtomicBool,
    ) -> Result<Option<Permit<'_>>, Error> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let (gate, limit) = match stage {
            Stage::Acb => (&self.acb, config.acb),
            Stage::Usm => (&self.usm, config.usm),
            Stage::Hca => (&self.hca, config.hca),
            Stage::Image => (&self.image, config.image),
        };
        limit
            .map(|limit| {
                gate.acquire(
                    limit,
                    cancel,
                    Instant::now() + Duration::from_secs(config.wait_timeout_seconds),
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
        for field in ["acb", "usm", "hca", "image"] {
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
