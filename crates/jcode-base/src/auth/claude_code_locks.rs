//! Cooperate with Claude Code's advisory locks while mutating its credential
//! file.
//!
//! Claude Code guards its OAuth refresh with npm `proper-lockfile`. The
//! protocol (verified against the 2.1.218 bundle, mirrored by cswap's
//! `claude_locks.py`):
//!
//! - The lock artifact is a directory; `mkdir` atomicity is the mutex.
//! - The refresh path takes two locks in order: `<config-home>/.oauth_refresh.lock`
//!   then the legacy `<config-home>.lock` (`~/.claude.lock`). On a contended
//!   legacy lock the primary is released before retrying, so waiters can never
//!   deadlock against each other.
//! - A lock is stale only past 60s (`stale: 60000`), and live holders touch it
//!   every 5s (`update: 5000`). A lock younger than 60s belongs to a live holder
//!   and must never be stolen.
//! - A held lock is retried 5 times with 1-2s jittered sleeps before giving up.
//!
//! Holding both locks while refreshing closes the dual-refresh race: Claude
//! Code's own double-checked re-read under the lock sees our freshly written
//! credential and aborts its refresh, and vice versa.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// A lock younger than this belongs to a live holder (`stale: 60000`).
pub const STALE_AFTER: Duration = Duration::from_secs(60);
/// Live holders refresh the lock mtime this often (`update: 5000`).
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(5);
/// Attempts before giving up on a held lock.
pub const MAX_ATTEMPTS: u32 = 5;
/// Jittered sleep between attempts, in milliseconds (1-2s).
pub const RETRY_DELAY_MS: (u64, u64) = (1000, 2000);

/// Claude Code's primary OAuth refresh lock: `<config-home>/.oauth_refresh.lock`.
pub fn oauth_refresh_lock_dir(config_home: &Path) -> PathBuf {
    config_home.join(".oauth_refresh.lock")
}

/// Legacy credential lock kept for external tools: `<config-home>.lock`.
pub fn legacy_lock_dir(config_home: &Path) -> PathBuf {
    let mut name = config_home
        .file_name()
        .map_or_else(std::ffi::OsString::new, ToOwned::to_owned);
    name.push(".lock");
    config_home.with_file_name(name)
}

/// One acquired `proper-lockfile` directory lock. Touches the directory every
/// [`TOUCH_INTERVAL`] while held and removes it on drop.
pub struct LockDir {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    toucher: Option<std::thread::JoinHandle<()>>,
}

impl LockDir {
    /// Try once: `mkdir`, taking over a lock whose mtime is older than
    /// [`STALE_AFTER`]. `Ok(None)` means a live holder owns it.
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create lock parent {}", parent.display()))?;
        }
        for _ in 0..2 {
            match std::fs::create_dir(path) {
                Ok(()) => return Ok(Some(Self::held(path))),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("create lock {}", path.display()));
                }
            }
            let Ok(held_mtime) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
                // Holder released between mkdir and stat; retry now.
                continue;
            };
            let age = SystemTime::now()
                .duration_since(held_mtime)
                .unwrap_or(Duration::ZERO);
            if age <= STALE_AFTER {
                return Ok(None);
            }
            // Dead holder per the protocol: remove and retake. Losing the
            // rmdir/mkdir race to another waiter just means reporting it held.
            if std::fs::remove_dir(path).is_err() {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn held(path: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let toucher = {
            let path = path.to_path_buf();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                // Poll in short slices so release is prompt.
                let slice = Duration::from_millis(100);
                let mut elapsed = Duration::ZERO;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(slice);
                    elapsed += slice;
                    if elapsed >= TOUCH_INTERVAL {
                        elapsed = Duration::ZERO;
                        if touch(&path).is_err() {
                            return; // lock stolen or removed; nothing to keep alive
                        }
                    }
                }
            })
        };
        Self {
            path: path.to_path_buf(),
            stop,
            toucher: Some(toucher),
        }
    }
}

fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.set_modified(SystemTime::now())
}

impl Drop for LockDir {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(toucher) = self.toucher.take()
            && toucher.join().is_err()
        {
            crate::logging::warn("Lock toucher thread panicked");
        }
        match std::fs::remove_dir(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                crate::logging::warn(&format!(
                    "Lock {} vanished while held (taken over as stale?)",
                    self.path.display()
                ))
            }
            Err(err) => crate::logging::warn(&format!(
                "Failed to release lock {}: {err}",
                self.path.display()
            )),
        }
    }
}

/// Both of Claude Code's credential-refresh locks, released together on drop
/// (legacy first, then primary: reverse acquisition order).
pub struct CredentialLocks {
    _legacy: LockDir,
    _primary: LockDir,
}

/// Hold Claude Code's credential-refresh locks in its own order, retrying a
/// held lock [`MAX_ATTEMPTS`] times with a jittered sleep drawn from
/// `retry_delay_ms` (production callers pass [`RETRY_DELAY_MS`]).
pub async fn acquire_credential_locks(
    config_home: &Path,
    retry_delay_ms: (u64, u64),
) -> Result<CredentialLocks> {
    let primary_dir = oauth_refresh_lock_dir(config_home);
    let legacy_dir = legacy_lock_dir(config_home);
    for attempt in 1..=MAX_ATTEMPTS {
        if let Some(primary) = LockDir::try_acquire(&primary_dir)? {
            if let Some(legacy) = LockDir::try_acquire(&legacy_dir)? {
                return Ok(CredentialLocks {
                    _legacy: legacy,
                    _primary: primary,
                });
            }
            // Contended legacy lock: release the primary before retrying, as
            // Claude Code does, so a waiting Claude Code never starves.
            drop(primary);
        }
        if attempt < MAX_ATTEMPTS {
            let (min, max) = retry_delay_ms;
            let delay = if max > min {
                rand::random_range(min..max)
            } else {
                min
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }
    anyhow::bail!(
        "Could not acquire Claude Code's credential locks in {} ({} attempts); another process is refreshing credentials",
        config_home.display(),
        MAX_ATTEMPTS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: (u64, u64) = (5, 10);

    #[test]
    fn lock_dir_names_follow_claude_code() {
        let home = Path::new("/home/u/.claude");
        assert_eq!(
            oauth_refresh_lock_dir(home),
            PathBuf::from("/home/u/.claude/.oauth_refresh.lock")
        );
        assert_eq!(legacy_lock_dir(home), PathBuf::from("/home/u/.claude.lock"));
    }

    #[tokio::test]
    async fn acquires_both_locks_and_releases_on_drop() {
        let temp = tempfile::TempDir::new().unwrap();
        let home = temp.path().join(".claude");
        let locks = acquire_credential_locks(&home, FAST).await.unwrap();
        assert!(oauth_refresh_lock_dir(&home).is_dir());
        assert!(legacy_lock_dir(&home).is_dir());
        drop(locks);
        assert!(!oauth_refresh_lock_dir(&home).exists());
        assert!(!legacy_lock_dir(&home).exists());
    }

    #[tokio::test]
    async fn live_holder_blocks_after_retries_and_leaves_primary_released() {
        let temp = tempfile::TempDir::new().unwrap();
        let home = temp.path().join(".claude");
        // Simulate a live Claude Code holding the legacy lock (fresh mtime).
        std::fs::create_dir_all(legacy_lock_dir(&home)).unwrap();
        let err = match acquire_credential_locks(&home, FAST).await {
            Ok(_) => panic!("a live legacy lock must block acquisition"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("another process is refreshing"));
        assert!(
            !oauth_refresh_lock_dir(&home).exists(),
            "primary must be released on legacy contention"
        );
    }

    #[tokio::test]
    async fn stale_lock_is_taken_over() {
        let temp = tempfile::TempDir::new().unwrap();
        let home = temp.path().join(".claude");
        let stale = oauth_refresh_lock_dir(&home);
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_AFTER - Duration::from_secs(5))
            .unwrap();
        let locks = acquire_credential_locks(&home, FAST).await.unwrap();
        drop(locks);
        assert!(!stale.exists());
    }
}
