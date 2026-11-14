//! Atomic file replacement and a streaming form of the core's content hash.

use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// FNV-1a fed in pieces; equals `eos_agent_core::content_hash` of the whole.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub(crate) fn finish(self) -> u64 {
        self.0
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `bytes` through a temp file and a rename, so readers
/// never see a half-written file. Writes through a symlink to its target and
/// keeps an existing file's permissions. Returns the new file's metadata.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<Metadata> {
    let target = real_target(path);
    let dir = target.parent().ok_or_else(|| io::Error::other("the path has no parent directory"))?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{name}.eos-{}-{seq}.tmp", std::process::id()));
    let result = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        if let Ok(old) = fs::metadata(&target) {
            file.set_permissions(old.permissions())?;
        }
        drop(file);
        fs::rename(&temp, &target)?;
        fs::metadata(&target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn real_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_hash_matches_core() {
        let data = b"hello\nworld\n";
        let mut h = Fnv::default();
        h.update(&data[..3]);
        h.update(&data[3..]);
        assert_eq!(h.finish(), eos_agent_core::content_hash(data));
    }
}
