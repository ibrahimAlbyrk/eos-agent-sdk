//! Claude Code's cross-process locks (proper-lockfile): creating a directory
//! is the atomic "take"; the holder touches it while working, and one nobody
//! touched for the stale time belonged to a process that died.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a holder marks its lock as alive (proper-lockfile `update`).
const TOUCH_EVERY: Duration = Duration::from_secs(5);

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    toucher: tokio::task::JoinHandle<()>,
}

/// proper-lockfile's lock for `target`: the directory `<target>.lock`.
pub fn lockfile_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

impl DirLock {
    /// One attempt to take the lock directory `path`; `Ok(None)` while
    /// another process holds it.
    pub async fn try_take(path: &Path, stale: Duration) -> std::io::Result<Option<Self>> {
        for _ in 0..2 {
            match tokio::fs::create_dir(path).await {
                Ok(()) => {
                    let touched = path.to_owned();
                    let toucher = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(TOUCH_EVERY).await;
                            let _ = touch(&touched);
                        }
                    });
                    return Ok(Some(Self { path: path.to_owned(), toucher }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let is_stale = tokio::fs::metadata(path)
                        .await
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > stale)
                        .unwrap_or(false);
                    if !is_stale {
                        return Ok(None);
                    }
                    let _ = tokio::fs::remove_dir(path).await;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Takes the lock, trying `attempts` times with `wait(attempt)` between tries.
    pub async fn take(
        path: &Path,
        stale: Duration,
        attempts: u32,
        wait: impl Fn(u32) -> Duration,
    ) -> std::io::Result<Option<Self>> {
        for attempt in 0..attempts {
            if let Some(lock) = Self::try_take(path, stale).await? {
                return Ok(Some(lock));
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(wait(attempt)).await;
            }
        }
        Ok(None)
    }

    /// Takes the lock, polling every `poll` for up to `total`.
    pub async fn wait_for(
        path: &Path,
        stale: Duration,
        poll: Duration,
        total: Duration,
    ) -> std::io::Result<Option<Self>> {
        let attempts = u32::try_from(total.as_millis() / poll.as_millis().max(1)).unwrap_or(u32::MAX).max(1) + 1;
        Self::take(path, stale, attempts, |_| poll).await
    }
}

fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.set_modified(std::time::SystemTime::now())
}

impl Drop for DirLock {
    fn drop(&mut self) {
        self.toucher.abort();
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_holder_at_a_time_and_stale_locks_are_taken_over() {
        let dir = std::env::temp_dir().join(format!("eos-oauth-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = lockfile_path(&dir.join("refresh-x"));
        assert!(path.ends_with("refresh-x.lock"));
        let held = DirLock::try_take(&path, Duration::from_secs(60)).await.expect("io").expect("free");
        assert!(path.is_dir());
        assert!(DirLock::try_take(&path, Duration::from_secs(60)).await.expect("io").is_none());
        let waited =
            DirLock::wait_for(&path, Duration::from_secs(60), Duration::from_millis(10), Duration::from_millis(30));
        assert!(waited.await.expect("io").is_none(), "still held");
        // Zero stale time: any existing lock counts as abandoned.
        std::mem::forget(held);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(DirLock::try_take(&path, Duration::ZERO).await.expect("io").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
