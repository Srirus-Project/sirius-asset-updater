use crate::Error;
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retry {
    pub attempts: usize,
    pub delay_ms: u64,
    pub max_delay_ms: u64,
}
impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay_ms: 250,
            max_delay_ms: 5000,
        }
    }
}
impl Retry {
    fn validate(&self) -> bool {
        (1..=8).contains(&self.attempts)
            && (1..=10_000).contains(&self.delay_ms)
            && (1..=30_000).contains(&self.max_delay_ms)
            && self.delay_ms <= self.max_delay_ms
    }
    pub fn retry(&self, error: &Error, attempt: usize) -> bool {
        attempt.saturating_add(1) < self.attempts
            && matches!(error, Error::Transport | Error::Status(429 | 500..=599))
    }
    pub fn delay(&self, attempt: usize) -> Duration {
        Duration::from_millis(
            self.delay_ms
                .saturating_mul(1u64.checked_shl(attempt as u32).unwrap_or(u64::MAX))
                .min(self.max_delay_ms),
        )
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub connect_timeout_ms: u64,
    pub download_timeout_ms: u64,
    pub snapshot_timeout_ms: u64,
    pub refresh_timeout_ms: u64,
    pub revalidate_interval_ms: u64,
    pub snapshot_retry: Retry,
    pub catalog_retry: Retry,
    pub asset_retry: Retry,
}
impl Default for Network {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 10_000,
            download_timeout_ms: 60_000,
            snapshot_timeout_ms: 10_000,
            refresh_timeout_ms: 30_000,
            revalidate_interval_ms: 120_000,
            snapshot_retry: Retry {
                delay_ms: 500,
                ..Retry::default()
            },
            catalog_retry: Retry::default(),
            asset_retry: Retry::default(),
        }
    }
}
impl Network {
    pub fn validate(&self) -> Result<(), Error> {
        if [
            self.connect_timeout_ms,
            self.download_timeout_ms,
            self.snapshot_timeout_ms,
            self.refresh_timeout_ms,
        ]
        .iter()
        .any(|v| !(100..=300_000).contains(v))
            || !(1000..=120_000).contains(&self.revalidate_interval_ms)
            || !self.snapshot_retry.validate()
            || !self.catalog_retry.validate()
            || !self.asset_retry.validate()
        {
            return Err(Error::Config);
        }
        Ok(())
    }
}
