//! Soft byte-weighted admission; source sizes are not a bound on decoded RSS.
use crate::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Condvar, Mutex,
};
pub(crate) struct Budget {
    limit: u64,
    used: Mutex<u64>,
    changed: Condvar,
}
pub(crate) struct Permit<'a> {
    budget: &'a Budget,
    weight: u64,
}
impl Budget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            changed: Condvar::new(),
        }
    }
    pub(crate) fn acquire(&self, bytes: u64, cancel: &AtomicBool) -> Result<Permit<'_>, Error> {
        if self.limit == 0 {
            return Err(Error::Config);
        }
        // An oversized resource gets exclusive admission instead of waiting forever.
        let weight = bytes.max(1).min(self.limit);
        let mut used = self.used.lock().map_err(|_| Error::Verification)?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if weight <= self.limit - *used {
                *used += weight;
                return Ok(Permit {
                    budget: self,
                    weight,
                });
            }
            used = self
                .changed
                .wait_timeout(used, std::time::Duration::from_millis(20))
                .map_err(|_| Error::Verification)?
                .0;
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used -= self.weight;
        self.budget.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weighted_admission_serializes_oversized_work_and_cancels_waiters() {
        let budget = Budget::new(10);
        let cancel = AtomicBool::new(false);
        let small = budget.acquire(4, &cancel).unwrap();
        let remaining = budget.acquire(6, &cancel).unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 10);
        drop(remaining);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| budget.acquire(u64::MAX, &cancel));
            std::thread::sleep(std::time::Duration::from_millis(40));
            assert!(!waiter.is_finished());
            cancel.store(true, Ordering::Relaxed);
            assert!(matches!(waiter.join().unwrap(), Err(Error::Cancelled)));
        });
        assert_eq!(*budget.used.lock().unwrap(), 4);
        drop(small);
        cancel.store(false, Ordering::Relaxed);
        let exclusive = budget.acquire(u64::MAX, &cancel).unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 10);
        drop(exclusive);
        assert_eq!(*budget.used.lock().unwrap(), 0);
        assert!(Budget::new(0).acquire(1, &cancel).is_err());
    }
}
