//! Approximate process-tree CPU feedback. This gates new work; it is not an OS quota.
use crate::Error;
use serde::Deserialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock, TryLockError,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub sample_ms: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_ms: 250,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        if !(50..=60_000).contains(&self.sample_ms) || (self.enabled && !cfg!(unix)) {
            return Err(Error::Config);
        }
        Ok(())
    }
}
fn check(cancel: &AtomicBool, deadline: Instant) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Export("CPU throttle timed out".into()));
    }
    Ok(())
}
fn pause(cancel: &AtomicBool, deadline: Instant, duration: Duration) -> Result<(), Error> {
    let until = (Instant::now() + duration).min(deadline);
    while Instant::now() < until {
        check(cancel, deadline)?;
        std::thread::sleep(
            Duration::from_millis(20).min(until.saturating_duration_since(Instant::now())),
        );
    }
    check(cancel, deadline)
}
fn wait_with(
    config: &Config,
    budget: usize,
    cancel: &AtomicBool,
    deadline: Instant,
    mut sample: impl FnMut() -> Result<f64, Error>,
) -> Result<(), Error> {
    if !config.enabled {
        return Ok(());
    }
    loop {
        check(cancel, deadline)?;
        let usage = sample()?;
        check(cancel, deadline)?;
        if !usage.is_finite() || usage < 0.0 {
            return Err(Error::Verification);
        }
        if usage < budget.max(1) as f64 * 100.0 {
            return Ok(());
        }
        pause(cancel, deadline, Duration::from_millis(config.sample_ms))?;
    }
}
pub(crate) fn wait(
    config: &Config,
    budget: usize,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<(), Error> {
    wait_with(config, budget, cancel, deadline, || {
        sample(config.sample_ms, cancel, deadline)
    })
}
#[derive(Default)]
struct Sampler {
    last: Option<Instant>,
    percent: f64,
    #[cfg(target_os = "linux")]
    previous: std::collections::BTreeMap<u32, (u64, u64)>,
}
fn sample(interval_ms: u64, cancel: &AtomicBool, deadline: Instant) -> Result<f64, Error> {
    static STATE: OnceLock<Mutex<Sampler>> = OnceLock::new();
    loop {
        check(cancel, deadline)?;
        match STATE.get_or_init(Default::default).try_lock() {
            Ok(mut state) => {
                if state
                    .last
                    .is_some_and(|last| last.elapsed() < Duration::from_millis(interval_ms))
                {
                    return Ok(state.percent);
                }
                let value = state.refresh(cancel, deadline)?;
                state.percent = value;
                state.last = Some(Instant::now());
                return Ok(value);
            }
            Err(TryLockError::WouldBlock) => pause(cancel, deadline, Duration::from_millis(20))?,
            Err(TryLockError::Poisoned(_)) => return Err(Error::Verification),
        }
    }
}
impl Sampler {
    #[cfg(target_os = "linux")]
    fn refresh(&mut self, cancel: &AtomicBool, deadline: Instant) -> Result<f64, Error> {
        use std::io::Read;
        let mut rows = Vec::new();
        for (index, entry) in std::fs::read_dir("/proc")
            .map_err(|_| Error::Io)?
            .enumerate()
        {
            if index > 100_000 {
                return Err(Error::Size);
            }
            check(cancel, deadline)?;
            let entry = entry.map_err(|_| Error::Io)?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(file) = std::fs::File::open(entry.path().join("stat")) else {
                continue;
            };
            let mut stat = String::new();
            if file.take(16_385).read_to_string(&mut stat).is_err() || stat.len() > 16_384 {
                continue;
            }
            if let Some((ppid, start, ticks)) = parse_stat(&stat) {
                rows.push((pid, ppid, start, ticks));
            }
        }
        let tree = tree_ticks(std::process::id(), &rows)?;
        // SAFETY: sysconf has no pointer arguments; an unavailable clock rate fails explicitly.
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks_per_second <= 0 {
            return Err(Error::Io);
        }
        let usage = self
            .last
            .map(|last| {
                delta_ticks(&self.previous, &tree) as f64
                    / ticks_per_second as f64
                    / last.elapsed().as_secs_f64().max(0.001)
                    * 100.0
            })
            .unwrap_or(0.0);
        self.previous = tree;
        Ok(usage)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    fn refresh(&mut self, cancel: &AtomicBool, deadline: Instant) -> Result<f64, Error> {
        use std::io::{Read, Seek};
        use std::process::{Command, Stdio};
        let mut output = tempfile::tempfile().map_err(|_| Error::Io)?;
        let mut child = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,pcpu="])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(output.try_clone().map_err(|_| Error::Io)?)
            .spawn()
            .map_err(|_| Error::Io)?;
        let sample_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        let result = (|| loop {
            check(cancel, sample_deadline)?;
            if output.metadata().map_err(|_| Error::Io)?.len() > 4 * 1024 * 1024 {
                return Err(Error::Size);
            }
            if let Some(status) = child.try_wait().map_err(|_| Error::Io)? {
                if !status.success() {
                    return Err(Error::Io);
                }
                output.rewind().map_err(|_| Error::Io)?;
                let mut bytes = String::new();
                (&mut output)
                    .take(4 * 1024 * 1024 + 1)
                    .read_to_string(&mut bytes)
                    .map_err(|_| Error::Io)?;
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(Error::Size);
                }
                return ps_percent(std::process::id(), child.id(), &bytes);
            }
            pause(cancel, sample_deadline, Duration::from_millis(20))?;
        })();
        // Always reap, including cancellation/timeout/read failure.
        if result.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        result
    }
    #[cfg(not(unix))]
    fn refresh(&mut self, _: &AtomicBool, _: Instant) -> Result<f64, Error> {
        Err(Error::Config)
    }
}
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<(u32, u64, u64)> {
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        fields.get(1)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
        fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .checked_add(fields.get(12)?.parse().ok()?)?,
    ))
}
#[cfg(any(target_os = "linux", test))]
fn tree_ticks(
    root: u32,
    rows: &[(u32, u32, u64, u64)],
) -> Result<std::collections::BTreeMap<u32, (u64, u64)>, Error> {
    let mut children = std::collections::BTreeMap::<u32, Vec<u32>>::new();
    let mut values = std::collections::BTreeMap::new();
    for &(pid, ppid, start, ticks) in rows {
        children.entry(ppid).or_default().push(pid);
        if values.insert(pid, (start, ticks)).is_some() {
            return Err(Error::Verification);
        }
    }
    if !values.contains_key(&root) {
        return Err(Error::Io);
    }
    let mut result = std::collections::BTreeMap::new();
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        if result.contains_key(&pid) {
            continue;
        }
        result.insert(pid, values[&pid]);
        if let Some(children) = children.get(&pid) {
            pending.extend(children);
        }
    }
    Ok(result)
}
#[cfg(any(target_os = "linux", test))]
fn delta_ticks(
    previous: &std::collections::BTreeMap<u32, (u64, u64)>,
    current: &std::collections::BTreeMap<u32, (u64, u64)>,
) -> u64 {
    current.iter().fold(0u64, |total, (pid, &(start, ticks))| {
        let delta = match previous.get(pid) {
            Some(&(old_start, old_ticks)) if old_start == start => ticks.saturating_sub(old_ticks),
            _ => ticks,
        };
        total.saturating_add(delta)
    })
}
#[cfg(any(all(unix, not(target_os = "linux")), test))]
fn ps_percent(root: u32, sampler: u32, text: &str) -> Result<f64, Error> {
    let mut rows = std::collections::BTreeMap::new();
    let mut children = std::collections::BTreeMap::<u32, Vec<u32>>::new();
    for line in text.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(Error::Verification);
        }
        let pid = parts[0].parse::<u32>().map_err(|_| Error::Verification)?;
        let ppid = parts[1].parse::<u32>().map_err(|_| Error::Verification)?;
        let usage = parts[2].parse::<f64>().map_err(|_| Error::Verification)?;
        if !usage.is_finite() || usage < 0.0 || rows.insert(pid, usage).is_some() {
            return Err(Error::Verification);
        }
        children.entry(ppid).or_default().push(pid);
    }
    if !rows.contains_key(&root) {
        return Err(Error::Io);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![root];
    let mut total = 0.0;
    while let Some(pid) = pending.pop() {
        if pid == sampler || !seen.insert(pid) {
            continue;
        }
        total += rows[&pid];
        if let Some(children) = children.get(&pid) {
            pending.extend(children);
        }
    }
    Ok(total)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_tree_sampling_ignores_unrelated_work_and_handles_exited_or_reused_pids() {
        let old = tree_ticks(
            10,
            &[
                (10, 1, 1, 100),
                (11, 10, 2, 500),
                (12, 10, 3, 20),
                (90, 1, 1, 99999),
            ],
        )
        .unwrap();
        let new = tree_ticks(
            10,
            &[
                (10, 1, 1, 130),
                (12, 10, 3, 25),
                (13, 12, 4, 7),
                (11, 10, 5, 4),
            ],
        )
        .unwrap();
        assert_eq!(delta_ticks(&old, &new), 46);
        assert_eq!(
            ps_percent(
                10,
                13,
                "10 1 20.0\n11 10 130.5\n12 11 40\n13 10 200\n90 1 9000\n"
            )
            .unwrap(),
            190.5
        );
        assert!(ps_percent(10, 13, "10 1 NaN").is_err());
        assert!(tree_ticks(99, &[(10, 1, 1, 0)]).is_err());
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[1] = "1";
        fields[11] = "4";
        fields[12] = "7";
        fields[19] = "99";
        assert_eq!(
            parse_stat(&format!("10 (name with ) spaces) {}", fields.join(" "))),
            Some((1, 99, 11))
        );
        assert!(parse_stat("broken").is_none());
    }
    #[test]
    fn high_usage_waits_then_recovers_and_deadline_cancel_and_failure_remain_bounded() {
        let cfg = Config {
            enabled: true,
            sample_ms: 50,
        };
        let cancel = AtomicBool::new(false);
        let mut values = [250.0, 120.0, 30.0].into_iter();
        wait_with(
            &cfg,
            1,
            &cancel,
            Instant::now() + Duration::from_secs(1),
            || Ok(values.next().unwrap()),
        )
        .unwrap();
        assert!(values.next().is_none());
        assert!(wait_with(
            &cfg,
            1,
            &cancel,
            Instant::now() + Duration::from_millis(60),
            || Ok(200.0)
        )
        .is_err());
        assert!(matches!(
            wait_with(
                &cfg,
                1,
                &cancel,
                Instant::now() + Duration::from_secs(1),
                || Err(Error::Io)
            ),
            Err(Error::Io)
        ));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                cancel.store(true, Ordering::Relaxed);
            });
            assert!(matches!(
                wait_with(
                    &cfg,
                    1,
                    &cancel,
                    Instant::now() + Duration::from_secs(1),
                    || Ok(200.0)
                ),
                Err(Error::Cancelled)
            ));
        });
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            wait_with(
                &cfg,
                1,
                &cancel,
                Instant::now() + Duration::from_secs(1),
                || panic!("must not sample")
            ),
            Err(Error::Cancelled)
        ));
        for ms in [0, 49, 60001] {
            assert!(Config {
                enabled: false,
                sample_ms: ms
            }
            .validate()
            .is_err());
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "bounded child fixture used only by the Linux load-response test"]
    fn load_child_fixture() {
        if std::env::var("SIRIUS_CPU_LOAD_CHILD").as_deref() != Ok("1") {
            return;
        }
        // Hard bound survives a failed parent, with no shell or external stress utility.
        let until = Instant::now() + Duration::from_secs(15);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(move || {
                    let mut value = 1u64;
                    while Instant::now() < until {
                        for _ in 0..10000 {
                            value = std::hint::black_box(
                                value.wrapping_mul(6364136223846793005).wrapping_add(1),
                            );
                        }
                    }
                    std::hint::black_box(value);
                });
            }
        });
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated Linux runtime with at least two available CPU cores"]
    fn linux_real_child_load_blocks_cancels_and_recovers() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let cancel = AtomicBool::new(false);
        let cfg = Config {
            enabled: true,
            sample_ms: 250,
        };
        let child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cpu_throttle::tests::load_child_fixture",
                    "--ignored",
                ])
                .env("SIRIUS_CPU_LOAD_CHILD", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut high = 0;
        let mut maximum = 0f64;
        while high < 3 {
            assert!(
                Instant::now() < deadline,
                "could not sustain more than one core of child CPU load"
            );
            let usage = sample(cfg.sample_ms, &cancel, deadline).unwrap();
            maximum = maximum.max(usage);
            high = if usage > 120.0 { high + 1 } else { 0 };
            if high < 3 {
                std::thread::sleep(Duration::from_millis(270));
            }
        }
        let started = Instant::now();
        assert!(
            matches!(wait(&cfg, 1, &cancel, Instant::now() + Duration::from_millis(700)), Err(Error::Export(message)) if message == "CPU throttle timed out")
        );
        assert!(started.elapsed() >= Duration::from_millis(650));
        assert!(started.elapsed() < Duration::from_secs(2));
        let blocked_ms = started.elapsed().as_millis();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                cancel.store(true, Ordering::Relaxed);
            });
            assert!(matches!(
                wait(&cfg, 1, &cancel, Instant::now() + Duration::from_secs(2)),
                Err(Error::Cancelled)
            ));
        });
        drop(child); // Reap before checking that admission resumes after load disappears.
        cancel.store(false, Ordering::Relaxed);
        let recovery = Instant::now();
        wait(&cfg, 1, &cancel, Instant::now() + Duration::from_secs(3)).unwrap();
        eprintln!(
            "real_child_load maximum_percent={maximum:.1} blocked_ms={} recovery_ms={}",
            blocked_ms,
            recovery.elapsed().as_millis()
        );
    }
    #[cfg(unix)]
    #[test]
    fn host_sampler_reads_real_process_tree_without_a_shell() {
        let mut sampler = Sampler::default();
        let usage = sampler
            .refresh(
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
        assert!(usage.is_finite() && usage >= 0.0);
    }
}
