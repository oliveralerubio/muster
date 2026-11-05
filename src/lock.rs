//! One exclusive mutation lock per machine (`<state_dir>/mutation.lock`).
//!
//! Every mutating command, and the revocation recovery that any command may
//! run, holds it, so a concurrent command can never delete or republish a
//! live revocation stage. The lock file is created with `create_new` and holds
//! the owner's pid and process start time; a lock whose owner is gone (pid
//! dead, or the pid now belongs to a younger process) is stale and is broken
//! under a second advisory lock so two waiters cannot both break it and then
//! both believe they own the new one.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::errors::MusterError;

pub const LOCK_FILE: &str = "mutation.lock";
const BREAK_FILE: &str = "mutation.lock.break";
/// How long a command waits for another one before giving up.
const DEFAULT_WAIT: Duration = Duration::from_secs(5);
/// A lock file that nobody has written yet is given this long before it counts as stale.
const WRITE_GRACE: Duration = Duration::from_secs(2);

thread_local! {
    static HELD_BY_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}
/// Threads of this process that hold the lock (the web panel runs commands on threads).
static IN_PROCESS_HOLDERS: Mutex<u32> = Mutex::new(0);

/// The lock, released on drop. A re-entrant acquisition on the same thread
/// holds nothing and releases nothing.
#[derive(Debug)]
pub struct MutationLock {
    path: Option<PathBuf>,
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
            HELD_BY_THIS_THREAD.with(|h| h.set(false));
            if let Ok(mut n) = IN_PROCESS_HOLDERS.lock() {
                *n = n.saturating_sub(1);
            }
        }
    }
}

/// Whether the current thread already holds the lock.
pub fn held_by_this_thread() -> bool {
    HELD_BY_THIS_THREAD.with(Cell::get)
}

pub fn wait_budget() -> Duration {
    std::env::var("MUSTER_LOCK_WAIT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_WAIT, Duration::from_millis)
}

/// Takes the lock, waiting briefly for another command. Fails with an
/// Operational error when it stays busy.
pub fn acquire(state_dir: &Path) -> Result<MutationLock, MusterError> {
    acquire_for(state_dir, wait_budget())
}

/// Like [`acquire`] with an explicit wait (zero: one attempt, no waiting).
pub fn acquire_for(state_dir: &Path, wait: Duration) -> Result<MutationLock, MusterError> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = try_acquire(state_dir)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(MusterError::Operational(
                "another muster command is running; wait for it to finish and retry".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One attempt. `Ok(None)` means a live command holds the lock. On a thread
/// that already holds it, returns a guard that owns nothing.
pub fn try_acquire(state_dir: &Path) -> Result<Option<MutationLock>, MusterError> {
    if held_by_this_thread() {
        return Ok(Some(MutationLock { path: None }));
    }
    create_dir_private(state_dir)?;
    let path = state_dir.join(LOCK_FILE);
    for _ in 0..3 {
        match create_lock(&path) {
            Ok(()) => {
                HELD_BY_THIS_THREAD.with(|h| h.set(true));
                if let Ok(mut n) = IN_PROCESS_HOLDERS.lock() {
                    *n += 1;
                }
                return Ok(Some(MutationLock { path: Some(path) }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !break_if_stale(state_dir, &path)? {
                    return Ok(None);
                }
            }
            Err(e) => return Err(io_err(&path, e)),
        }
    }
    Ok(None)
}

fn create_lock(path: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let pid = std::process::id();
    let started = process_start(pid).unwrap_or(0);
    file.write_all(format!("{pid} {started}\n").as_bytes())?;
    file.sync_all()
}

/// Removes the lock when its owner is gone. Returns whether it did.
fn break_if_stale(state_dir: &Path, path: &Path) -> Result<bool, MusterError> {
    // Serialize breakers: re-read under the guard, so a lock another waiter
    // just created is never removed on the strength of a stale view.
    let guard_path = state_dir.join(BREAK_FILE);
    let guard = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&guard_path)
        .map_err(|e| io_err(&guard_path, e))?;
    guard.lock().map_err(|e| io_err(&guard_path, e))?;
    let stale = is_stale(path);
    if stale {
        let _ = fs::remove_file(path);
    }
    drop(guard);
    Ok(stale)
}

fn is_stale(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        // Gone already (the owner just finished): the next create decides.
        return !path.exists();
    };
    let mut parts = text.split_whitespace();
    let (Some(pid), Some(started)) = (
        parts.next().and_then(|p| p.parse::<u32>().ok()),
        parts.next().and_then(|s| s.parse::<u64>().ok()),
    ) else {
        // Empty or partial: its creator may be mid-write.
        return fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > WRITE_GRACE);
    };
    if pid == std::process::id() {
        // Ours: live only while a thread of this process holds it.
        return IN_PROCESS_HOLDERS.lock().map_or(true, |n| *n == 0);
    }
    if !process_alive(pid) {
        return true;
    }
    // The pid is alive; a different start time means it was reused.
    started != 0 && process_start(pid).is_some_and(|now| now != started)
}

fn process_alive(pid: u32) -> bool {
    if Path::new("/proc/self").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Process start time in clock ticks since boot (Linux), a stand-in elsewhere
/// where `/proc` is missing (`ps` start time hashed to a number).
fn process_start(pid: u32) -> Option<u64> {
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Field 22, counted after the command name's closing parenthesis.
        let rest = stat.rsplit_once(')')?.1;
        return rest.split_whitespace().nth(19)?.parse().ok();
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(
        text.bytes().fold(1469598103934665603u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(1099511628211)
        }) | 1,
    )
}

fn create_dir_private(dir: &Path) -> Result<(), MusterError> {
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))
}

fn io_err(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquire_from_another_holder_is_refused_until_release() {
        let dir = tempdir().unwrap();
        // A lock owned by a live foreign process (our parent) is respected.
        let parent = std::os::unix::process::parent_id();
        let started = process_start(parent).unwrap_or(0);
        fs::write(dir.path().join(LOCK_FILE), format!("{parent} {started}\n")).unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_dead_owner_makes_the_lock_stale() {
        let dir = tempdir().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        fs::write(dir.path().join(LOCK_FILE), format!("{pid} 1\n")).unwrap();
        let lock = try_acquire(dir.path()).unwrap().expect("stale lock broken");
        assert!(dir.path().join(LOCK_FILE).exists());
        drop(lock);
        assert!(!dir.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn a_reused_pid_with_another_start_time_is_stale() {
        let dir = tempdir().unwrap();
        let parent = std::os::unix::process::parent_id();
        let Some(real) = process_start(parent) else {
            return;
        };
        fs::write(
            dir.path().join(LOCK_FILE),
            format!("{parent} {}\n", real + 12345),
        )
        .unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_some());
    }

    #[test]
    fn same_thread_reacquire_is_free_and_release_removes_the_file() {
        let dir = tempdir().unwrap();
        let outer = acquire(dir.path()).unwrap();
        let inner = acquire(dir.path()).unwrap();
        drop(inner);
        assert!(dir.path().join(LOCK_FILE).exists());
        drop(outer);
        assert!(!dir.path().join(LOCK_FILE).exists());
    }
}
