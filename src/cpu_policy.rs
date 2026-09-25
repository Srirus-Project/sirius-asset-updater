//! Export worker sizing and aggregate CPU-stage budgets; not a CPU usage quota.
use crate::Error;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto_tune: bool,
    pub limit_stages: bool,
    pub budget_auto: bool,
    pub budget_ratio: f64,
    pub reserved: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_tune: false,
            limit_stages: false,
            budget_auto: true,
            budget_ratio: 1.0,
            reserved: 0,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if !self.budget_ratio.is_finite() || self.budget_ratio <= 0.0 || self.budget_ratio > 1.0 {
            return Err(Error::Config);
        }
        Ok(())
    }
    /// Configured concurrency is a floor, capped at twice available CPUs and 64.
    /// This mirrors the generic Haruki post-process policy, without its stage heuristics.
    pub fn workers_for_cpus(&self, configured: usize, cpus: usize) -> Result<usize, Error> {
        self.validate()?;
        if !(1..=64).contains(&configured) {
            return Err(Error::Config);
        }
        if !self.auto_tune {
            return Ok(configured);
        }
        let cpus = cpus.max(1);
        let budget = self.budget_for_cpus(cpus)?;
        Ok(configured.max(budget).min(cpus.saturating_mul(2)).min(64))
    }
    pub fn budget_for_cpus(&self, cpus: usize) -> Result<usize, Error> {
        self.validate()?;
        let cpus = cpus.max(1);
        let budget = if self.budget_auto {
            ((cpus as f64 * self.budget_ratio).floor() as usize)
                .saturating_sub(self.reserved)
                .max(1)
        } else {
            cpus
        };
        Ok(budget)
    }
    pub fn workers(&self, configured: usize) -> Result<usize, Error> {
        self.workers_for_cpus(
            configured,
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        )
    }
}

pub(crate) fn available_cpus() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizing_preserves_manual_policy_and_scales_to_host_budget() {
        let mut config = Config::default();
        for cpus in [0, 1, 8, 64, usize::MAX] {
            assert_eq!(config.workers_for_cpus(4, cpus).unwrap(), 4);
        }
        config.auto_tune = true;
        assert_eq!(config.workers_for_cpus(4, 64).unwrap(), 64);
        config.budget_ratio = 0.5;
        config.reserved = 2;
        assert_eq!(config.workers_for_cpus(4, 64).unwrap(), 30);
        assert_eq!(config.workers_for_cpus(4, 8).unwrap(), 4);
        assert_eq!(config.workers_for_cpus(64, 1).unwrap(), 2);
        config.reserved = usize::MAX;
        assert_eq!(config.workers_for_cpus(1, 0).unwrap(), 1);
        config.budget_auto = false;
        assert_eq!(config.workers_for_cpus(4, 64).unwrap(), 64);
        assert_eq!(config.workers_for_cpus(4, usize::MAX).unwrap(), 64);
    }
    #[test]
    fn invalid_policy_fails_even_when_automatic_sizing_is_disabled() {
        for ratio in [0.0, -1.0, 1.01, f64::NAN, f64::INFINITY] {
            assert!(Config {
                budget_ratio: ratio,
                ..Config::default()
            }
            .workers_for_cpus(4, 8)
            .is_err());
        }
        for workers in [0, 65, usize::MAX] {
            assert!(Config::default().workers_for_cpus(workers, 8).is_err());
        }
        let config: Config = yaml_serde::from_str("{}").unwrap();
        assert_eq!(config.workers_for_cpus(4, 64).unwrap(), 4);
        assert!(yaml_serde::from_str::<Config>("unknown: true").is_err());
    }
}
