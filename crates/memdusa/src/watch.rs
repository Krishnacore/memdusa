//! Background sync watcher plumbing: a per-MEMDUSA_HOME lock, exponential
//! backoff, the loop itself (testable with a fake clock), and the "last sync"
//! timestamp used by opportunistic sync.
//!
//! The loop never sleeps in tests: [`run_loop`] is generic over a [`Clock`] and
//! a [`Runner`], so the unit tests drive it with a clock that records delays and
//! returns immediately.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use memdusa_core::model::{Millis, now_ms};
use tokio::sync::Notify;

use crate::config;

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);
pub const BASE_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Opportunistic sync after a hook is skipped when a sync ran this recently.
pub const QUIET_COOLDOWN: Millis = 10_000;

/// `15s`, `30`, `2m`, `500ms`, `1h` — enough for `--interval`.
pub fn parse_interval(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty sync interval");
    let (n, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (s, 1000)
    };
    let n: u64 = n.parse().with_context(|| format!("invalid sync interval '{s}'"))?;
    anyhow::ensure!(n > 0, "sync interval must be positive");
    Ok(Duration::from_millis(n.saturating_mul(mult)))
}

// ---------------------------------------------------------------- backoff

/// Exponential backoff, capped at [`MAX_BACKOFF`]. A successful sync resets it
/// to the base so the next failure starts from one second again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    current: Duration,
    base: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self { current: base, base, max }
    }

    /// Delay after a successful pass: the normal polling interval.
    pub fn on_success(&mut self, interval: Duration) -> Duration {
        self.current = self.base;
        interval
    }

    /// Delay after a failed pass: 1s, 2s, 4s, ... capped at `max`.
    pub fn on_error(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(2).min(self.max);
        delay
    }
}

// ---------------------------------------------------------------- loop

/// One sync pass. Kept as a trait so the loop can be unit-tested without a
/// server or a local database.
pub trait Runner {
    async fn sync_all(&self) -> anyhow::Result<(usize, usize)>;
}

/// Sleep abstraction. The fake clock in tests records delays instead of
/// waiting; the real clock also wakes on a shutdown notification.
pub trait Clock {
    async fn sleep(&self, d: Duration);
}

pub struct RealClock {
    notify: Arc<Notify>,
}

impl RealClock {
    pub fn new(notify: Arc<Notify>) -> Self {
        Self { notify }
    }
}

impl Clock for RealClock {
    async fn sleep(&self, d: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = self.notify.notified() => {}
        }
    }
}

/// Run sync passes until `stop` is set. One log line per pass with changes;
/// quiet when nothing moved. Errors back off exponentially.
pub async fn run_loop<R: Runner, C: Clock>(runner: &R, clock: &C, interval: Duration, label: &str, stop: &AtomicBool) {
    let mut backoff = Backoff::new(BASE_BACKOFF, MAX_BACKOFF);
    while !stop.load(Ordering::Relaxed) {
        match runner.sync_all().await {
            Ok((pushed, pulled)) => {
                if pushed + pulled > 0 {
                    println!("{label}: pushed {pushed}, pulled {pulled}");
                }
                let delay = backoff.on_success(interval);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                clock.sleep(delay).await;
            }
            Err(e) => {
                eprintln!("{label}: sync failed: {e:#}");
                let delay = backoff.on_error();
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                clock.sleep(delay).await;
            }
        }
    }
}

// ---------------------------------------------------------------- lock file

/// A single watcher per MEMDUSA_HOME. The lock holds the pid so `sync status`
/// can report whether a watcher is running.
pub struct WatchLock {
    path: PathBuf,
    pid: u32,
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` probes for existence without sending a signal.
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!(" {pid} "))).unwrap_or(false)
    }
}

impl WatchLock {
    pub fn path() -> anyhow::Result<PathBuf> {
        Ok(config::dir()?.join("sync-watch.lock"))
    }

    /// Pid of a live watcher, if any (the file may belong to a dead process).
    pub fn pid() -> anyhow::Result<Option<u32>> {
        let path = Self::path()?;
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(text.trim().parse::<u32>().ok().filter(|p| pid_alive(*p)))
    }

    /// Create the lock, or return `None` when another live watcher owns it.
    /// A stale lock (dead pid) is removed and retried once.
    pub fn try_acquire() -> anyhow::Result<Option<Self>> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let pid = std::process::id();
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    write!(f, "{pid}")?;
                    return Ok(Some(Self { path, pid }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::pid()?.is_some() {
                        return Ok(None);
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        Ok(None)
    }
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        // Never remove another process's lock.
        if fs::read_to_string(&self.path).map(|s| s.trim() == self.pid.to_string()).unwrap_or(false) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------- last sync

fn stamp_path() -> anyhow::Result<PathBuf> {
    Ok(config::dir()?.join("sync.last"))
}

pub fn note_sync() -> anyhow::Result<()> {
    let path = stamp_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, now_ms().to_string())?;
    Ok(())
}

pub fn last_sync() -> Option<Millis> {
    fs::read_to_string(stamp_path().ok()?).ok()?.trim().parse().ok()
}

/// Whether a detached opportunistic sync should run now: no live watcher and
/// no sync in the last ten seconds. `MEMDUSA_SYNC_OPPORTUNISTIC=0` disables it.
pub fn should_run_quiet() -> anyhow::Result<bool> {
    if std::env::var("MEMDUSA_SYNC_OPPORTUNISTIC").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no")) {
        return Ok(false);
    }
    if WatchLock::pid()?.is_some() {
        return Ok(false);
    }
    Ok(last_sync().is_none_or(|t| now_ms().saturating_sub(t) >= QUIET_COOLDOWN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn intervals_parse() {
        assert_eq!(parse_interval("15s").unwrap(), Duration::from_secs(15));
        assert_eq!(parse_interval("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_interval("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_interval("500ms").unwrap(), Duration::from_millis(500));
        assert!(parse_interval("0s").is_err());
        assert!(parse_interval("nope").is_err());
    }

    #[test]
    fn backoff_grows_and_caps_then_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        assert_eq!(b.on_error(), Duration::from_secs(1));
        assert_eq!(b.on_error(), Duration::from_secs(2));
        assert_eq!(b.on_error(), Duration::from_secs(4));
        assert_eq!(b.on_error(), Duration::from_secs(5));
        assert_eq!(b.on_error(), Duration::from_secs(5));
        assert_eq!(b.on_success(Duration::from_secs(15)), Duration::from_secs(15));
        assert_eq!(b.on_error(), Duration::from_secs(1));
    }

    #[derive(Default)]
    struct FakeClock {
        delays: Mutex<Vec<Duration>>,
    }

    impl Clock for FakeClock {
        async fn sleep(&self, d: Duration) {
            self.delays.lock().unwrap().push(d);
        }
    }

    struct FakeRunner {
        count: Mutex<usize>,
        stop: AtomicBool,
    }

    impl Runner for FakeRunner {
        async fn sync_all(&self) -> anyhow::Result<(usize, usize)> {
            let mut n = self.count.lock().unwrap();
            *n += 1;
            let n = *n;
            if n == 6 {
                self.stop.store(true, Ordering::Relaxed);
            }
            if n <= 2 {
                anyhow::bail!("server down")
            }
            Ok((1, 2))
        }
    }

    #[tokio::test]
    async fn loop_backs_off_on_errors_and_sleeps_the_interval_on_success() {
        let runner = FakeRunner { count: Mutex::new(0), stop: AtomicBool::new(false) };
        let clock = FakeClock::default();
        run_loop(&runner, &clock, Duration::from_secs(15), "test", &runner.stop).await;
        let delays = clock.delays.lock().unwrap().clone();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(15),
                Duration::from_secs(15),
                Duration::from_secs(15),
                // no sleep after the 6th pass: stop was requested during it, so the loop exits at once
            ],
            "{delays:?}"
        );
    }

    #[test]
    fn lock_is_single_instance_and_releases_on_drop() {
        use config::TEST_ENV_LOCK;
        let _env = TEST_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("memdusa-watch-{}", memdusa_core::model::new_id()));
        let old = std::env::var_os("MEMDUSA_HOME");
        unsafe {
            std::env::set_var("MEMDUSA_HOME", &dir);
        }
        let first = WatchLock::try_acquire().unwrap().expect("first lock acquired");
        // The lock holds this very process's pid, so a second acquire fails.
        assert!(WatchLock::try_acquire().unwrap().is_none());
        drop(first);
        assert!(WatchLock::pid().unwrap().is_none());
        assert!(WatchLock::try_acquire().unwrap().is_some());
        unsafe {
            match old {
                Some(v) => std::env::set_var("MEMDUSA_HOME", v),
                None => std::env::remove_var("MEMDUSA_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
