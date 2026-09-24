//! Shared admission control for media subprocesses within one export configuration.
use crate::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Condvar, Mutex,
};
use std::time::{Duration, Instant};
#[derive(Default)]
pub(crate) struct Gate {
    active: Mutex<usize>,
    changed: Condvar,
}
pub(crate) struct Permit<'a>(&'a Gate);
impl Gate {
    pub(crate) fn acquire(
        &self,
        limit: usize,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Permit<'_>, Error> {
        if limit == 0 {
            return Err(Error::Config);
        }
        let mut active = self.active.lock().map_err(|_| Error::Verification)?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Export("media process timed out".into()));
            }
            if *active < limit {
                *active += 1;
                return Ok(Permit(self));
            }
            active = self
                .changed
                .wait_timeout(active, Duration::from_millis(20).min(deadline - now))
                .map_err(|_| Error::Verification)?
                .0;
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .0
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *active = active.saturating_sub(1);
        self.0.changed.notify_one();
    }
}
