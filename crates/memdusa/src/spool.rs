//! Offline queue for remote hooks, plus a cached brief for SessionStart.
//!
//! When the server can't be reached, write-only hook events are appended to
//! `<MEMDUSA_HOME>/spool/queue.jsonl` instead of being lost. The next hook
//! that runs while the server is reachable replays the backlog first, in one
//! batch request, sorted by the time each hook fired.
//!
//! Concurrency: several hook processes can run at once (parallel tool calls).
//! Appends are single `write` calls on an O_APPEND file. A flush first renames
//! `queue.jsonl` to a unique `pending-*.jsonl` (atomic), and only one process
//! flushes at a time (`flush.lock`). Pending files are deleted only after the
//! server confirms, so delivery is at-least-once.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use memdusa_core::model::{Millis, new_id, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::hooks::{HookBatchItem, HookResponse};
use crate::remote::{Client, RemoteError};

/// Stop queueing past this size: something is badly wrong, don't fill the disk.
const MAX_QUEUE_BYTES: u64 = 20 * 1024 * 1024;
const BATCH_SIZE: usize = 200;
/// A flush lock older than this belongs to a process that died.
const STALE_LOCK: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Server the hook was meant for; entries are only replayed there.
    pub url: String,
    #[serde(flatten)]
    pub item: HookBatchItem,
}

pub struct Spool {
    dir: PathBuf,
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Removes the flush lock when dropped, even on early return.
struct FlushLock(PathBuf);

impl Drop for FlushLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Spool {
    pub fn open() -> anyhow::Result<Self> {
        Self::at(config::dir()?.join("spool"))
    }

    pub fn at(dir: PathBuf) -> anyhow::Result<Self> {
        private_dir(&dir)?;
        Ok(Self { dir })
    }

    fn queue_path(&self) -> PathBuf {
        self.dir.join("queue.jsonl")
    }

    fn pending_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("pending-")))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    pub fn push(&self, url: &str, item: HookBatchItem) -> anyhow::Result<()> {
        let path = self.queue_path();
        if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_QUEUE_BYTES {
            anyhow::bail!("offline queue is full ({}); dropping this event", path.display());
        }
        let mut line = serde_json::to_string(&Entry { url: url.to_owned(), item })?;
        line.push('\n');
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        f.write_all(line.as_bytes())?; // one write: concurrent appends don't interleave
        Ok(())
    }

    /// Number of queued hook events (all servers).
    pub fn len(&self) -> usize {
        let mut files = self.pending_files();
        files.push(self.queue_path());
        files
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .map(|s| s.lines().filter(|l| !l.is_empty()).count())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Option<FlushLock> {
        let path = self.dir.join("flush.lock");
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Some(FlushLock(path));
                }
                Err(_) => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > STALE_LOCK)
                        .unwrap_or(true);
                    if !stale {
                        return None; // another process is flushing right now
                    }
                    let _ = fs::remove_file(&path);
                }
            }
        }
        None
    }

    /// Replay everything queued for `url`. Returns how many events were
    /// delivered. On failure, undelivered events stay queued.
    pub async fn flush(&self, client: &Client, url: &str) -> Result<usize, RemoteError> {
        let local = |e: anyhow::Error| RemoteError {
            retryable: false,
            needs_login: false,
            message: format!("offline queue: {e:#}"),
        };
        let Some(_lock) = self.lock() else { return Ok(0) };

        let queue = self.queue_path();
        if queue.exists() {
            fs::rename(&queue, self.dir.join(format!("pending-{}.jsonl", new_id()))).map_err(|e| local(e.into()))?;
        }
        let files = self.pending_files();
        if files.is_empty() {
            return Ok(0);
        }
        let (mut mine, mut others) = (vec![], vec![]);
        for f in &files {
            for line in fs::read_to_string(f).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<Entry>(line) {
                    Ok(e) if e.url == url => mine.push(e),
                    Ok(e) => others.push(e),
                    Err(e) => eprintln!("memdusa: dropping unreadable queued event: {e}"),
                }
            }
        }
        mine.sort_by_key(|e| e.item.request.at.unwrap_or(0));

        let mut sent = 0;
        let mut failure = None;
        for chunk in mine.chunks(BATCH_SIZE) {
            let items: Vec<HookBatchItem> = chunk.iter().map(|e| e.item.clone()).collect();
            match client.hook_batch(&items).await {
                Ok(resp) => {
                    for r in &resp.rejected {
                        eprintln!("memdusa: server rejected a queued event: {r}");
                    }
                    sent += chunk.len();
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }

        // Rewrite whatever wasn't delivered (and other servers' entries), then drop the old files.
        let keep: Vec<&Entry> = mine.iter().skip(sent).chain(others.iter()).collect();
        if !keep.is_empty() {
            let text: String = keep.iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
            fs::write(self.dir.join(format!("pending-{}.jsonl", new_id())), text).map_err(|e| local(e.into()))?;
        }
        for f in files {
            let _ = fs::remove_file(f);
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(sent),
        }
    }
}

// ---------------------------------------------------------------- brief cache

#[derive(Serialize, Deserialize)]
struct CachedBrief {
    saved_at: Millis,
    output: String,
}

fn cache_path(project: &str) -> anyhow::Result<PathBuf> {
    let name: String = project.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect();
    Ok(config::dir()?.join("cache").join(format!("{name}.json")))
}

/// Remember the last SessionStart output, for when the server is unreachable.
pub fn cache_brief(project: &str, output: &str) -> anyhow::Result<()> {
    let path = cache_path(project)?;
    private_dir(path.parent().expect("cache dir"))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string(&CachedBrief { saved_at: now_ms(), output: output.into() })?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// The cached SessionStart output, with a note that it may be out of date.
pub fn cached_brief(project: &str, why: &str) -> Option<HookResponse> {
    let cached: CachedBrief = serde_json::from_str(&fs::read_to_string(cache_path(project).ok()?).ok()?).ok()?;
    let mut out: Value = serde_json::from_str(&cached.output).ok()?;
    let ctx = out.pointer_mut("/hookSpecificOutput/additionalContext")?;
    let mins = (now_ms() - cached.saved_at).max(0) / 60_000;
    let note = format!(
        "NOTE: the memdusa server is unreachable ({why}). This is the brief cached {mins} min ago and may be out of \
         date. Your activity is queued locally and will sync when the server is back.\n\n"
    );
    *ctx = Value::String(format!("{note}{}", ctx.as_str().unwrap_or_default()));
    Some(HookResponse { output: Some(out.to_string()), files_changed: false })
}
