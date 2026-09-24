//! Per-operation cooperative cancellation shared with FFmpeg interrupt callbacks.
use super::MediaError;
use std::{
    cell::RefCell,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
thread_local! { static CURRENT: RefCell<Option<Arc<Control>>> = const { RefCell::new(None) }; }
pub(super) struct Control {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}
impl Control {
    fn check(&self) -> Result<(), MediaError> {
        if self.cancel.load(Ordering::Acquire) {
            Err(MediaError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(MediaError::Timeout)
        } else {
            Ok(())
        }
    }
}
pub(super) fn current() -> Option<Arc<Control>> {
    CURRENT.with(|c| c.borrow().clone())
}
pub(super) fn check() -> Result<(), MediaError> {
    current().map_or(Ok(()), |c| c.check())
}
pub(super) unsafe extern "C" fn interrupt(opaque: *mut c_void) -> i32 {
    if opaque.is_null() {
        return 0;
    }
    i32::from(unsafe { &*opaque.cast::<Control>() }.check().is_err())
}
struct Scope;
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
    }
}
/// Run synchronous codec work with cancellation and a shared absolute deadline.
/// CPU work checks between codec operations; an OS-blocked file operation is not preemptible.
pub fn controlled<T>(
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    work: impl FnOnce() -> Result<T, MediaError>,
) -> Result<T, MediaError> {
    if current().is_some() {
        return Err(MediaError::Media {
            message: "nested media control is unsupported".into(),
        });
    }
    CURRENT.with(|c| *c.borrow_mut() = Some(Arc::new(Control { cancel, deadline })));
    let _scope = Scope;
    check()?;
    let result = work();
    check()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupt_control_is_shared_across_threads_and_scope_unwinds() {
        let cancel = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = std::panic::catch_unwind(|| {
            let _ = controlled(cancel.clone(), deadline, || -> Result<(), MediaError> {
                let control = current().unwrap();
                cancel.store(true, Ordering::Release);
                assert_eq!(
                    std::thread::spawn(move || unsafe {
                        interrupt(Arc::as_ptr(&control).cast_mut().cast())
                    })
                    .join()
                    .unwrap(),
                    1
                );
                panic!("synthetic unwind");
            });
        });
        assert!(result.is_err());
        assert!(current().is_none());
        assert!(check().is_ok());
    }
}
