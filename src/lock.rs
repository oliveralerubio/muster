//! One exclusive mutation lock per machine (`<state_dir>/mutation.lock`).
//!
//! Every mutating command, and the revocation recovery that any command may
//! run, holds it, so a concurrent command can never delete or republish a
//! live revocation stage. The lock is a kernel advisory lock on an open file
//! (the same mechanism as the sync and secret-lease locks), held for the whole
//! mutation or recovery. The kernel releases it when the holder exits, so
//! there is no owner bookkeeping, no age cap, and a leftover or planted file
//! with nobody holding it is simply free. The file is never unlinked, so a
//! guard can never remove somebody else's lock.

use std::cell::Cell;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::errors::MusterError;

pub const LOCK_FILE: &str = "mutation.lock";
/// How long a command waits for another one before giving up.
const DEFAULT_WAIT: Duration = Duration::from_secs(5);

thread_local! {
    static HELD_BY_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// The lock, released on drop. A re-entrant acquisition on the same thread
/// holds nothing and releases nothing.
#[derive(Debug)]
pub struct MutationLock {
    file: Option<File>,
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            HELD_BY_THIS_THREAD.with(|h| h.set(false));
            // Closing the file releases the lock; unlocking first is explicit.
            let _ = file.unlock();
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
        return Ok(Some(MutationLock { file: None }));
    }
    std::fs::create_dir_all(state_dir).map_err(|e| io_err(state_dir, e))?;
    let path = state_dir.join(LOCK_FILE);
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&path).map_err(|e| io_err(&path, e))?;
    match file.try_lock() {
        Ok(()) => {
            HELD_BY_THIS_THREAD.with(|h| h.set(true));
            Ok(Some(MutationLock { file: Some(file) }))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(io_err(&path, e)),
    }
}

fn io_err(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[test]
    fn a_second_holder_is_refused_until_release() {
        let dir = tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let state = dir.path().to_path_buf();
        let holder = std::thread::spawn(move || {
            let lock = try_acquire(&state).unwrap().expect("free");
            tx.send(()).unwrap();
            done_rx.recv().unwrap();
            drop(lock);
        });
        rx.recv().unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_none());
        done_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_some());
    }

    #[test]
    fn a_planted_file_nobody_holds_is_free() {
        let dir = tempdir().unwrap();
        let parent = std::os::unix::process::parent_id();
        // Whatever the content says (a live pid, a fresh mtime), no held lock
        // means no owner.
        std::fs::write(dir.path().join(LOCK_FILE), format!("{parent} 1\n")).unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_some());
        std::fs::write(dir.path().join(LOCK_FILE), "garbage").unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_some());
    }

    #[test]
    fn an_old_lock_that_is_still_held_stays_held() {
        let dir = tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let lock = try_acquire(&state).unwrap().expect("free");
            tx.send(()).unwrap();
            done_rx.recv().unwrap();
            drop(lock);
        });
        rx.recv().unwrap();
        let old = SystemTime::now() - Duration::from_secs(24 * 3600);
        File::options()
            .write(true)
            .open(dir.path().join(LOCK_FILE))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_none());
        done_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn dropping_a_guard_never_removes_the_file_or_another_lock() {
        let dir = tempdir().unwrap();
        let first = acquire(dir.path()).unwrap();
        drop(first);
        assert!(dir.path().join(LOCK_FILE).exists());
        let second = acquire(dir.path()).unwrap();
        // A re-entrant guard dropping releases nothing.
        let inner = acquire(dir.path()).unwrap();
        drop(inner);
        assert!(held_by_this_thread());
        drop(second);
        assert!(!held_by_this_thread());
    }

    #[test]
    fn a_child_process_holding_the_lock_blocks_us_until_it_exits() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        std::fs::write(&path, "").unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 9>>\"$1\"; flock -n 9 || exit 3; echo ready; exec sleep 30")
            .arg("sh")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .spawn();
        let Ok(child) = child.as_mut() else { return };
        let mut line = String::new();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        std::io::BufRead::read_line(&mut out, &mut line).unwrap();
        if line.trim() != "ready" {
            // No flock(1) on this machine; the in-process tests cover the rest.
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        assert!(try_acquire(dir.path()).unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_some());
    }
}
