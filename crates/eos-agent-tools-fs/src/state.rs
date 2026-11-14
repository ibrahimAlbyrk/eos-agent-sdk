//! What the agent has seen of each file, for read-before-write and staleness.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use eos_agent_core::{Extensions, FileStamp, FileStates, ToolContext, content_hash};

use crate::common::mtime_ms;

/// Files whose last Read was cut by the size/token cap (Claude Code's
/// `isPartialView`). Unlike an offset/limit read, such a view blocks Edit and
/// Write until the file is read again. Bounded like [`FileStates`].
#[derive(Debug, Default)]
struct TruncatedViews(Mutex<VecDeque<PathBuf>>);

const TRUNCATED_CAPACITY: usize = 256;

impl TruncatedViews {
    fn set(&self, path: &Path, truncated: bool) {
        let mut paths = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        paths.retain(|p| p != path);
        if truncated {
            paths.push_back(path.to_path_buf());
            if paths.len() > TRUNCATED_CAPACITY {
                paths.pop_front();
            }
        }
    }

    fn contains(&self, path: &Path) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|p| p == path)
    }
}

pub(crate) fn stamp(meta: &std::fs::Metadata, hash: u64, partial: bool) -> FileStamp {
    FileStamp { modified_ms: mtime_ms(meta), len: meta.len(), hash, partial }
}

pub(crate) fn record(ctx: &ToolContext<'_>, path: &Path, stamp: FileStamp, truncated: bool) {
    record_in(ctx.extensions, path, stamp, truncated);
}

pub(crate) fn record_in(extensions: &Extensions, path: &Path, stamp: FileStamp, truncated: bool) {
    extensions.get_or_default::<TruncatedViews>().set(path, truncated);
    extensions.get_or_default::<FileStates>().record(path.to_path_buf(), stamp);
}

/// What the agent's last look at a file says about writing it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prior {
    Unread,
    /// The last Read was cut by the cap.
    Truncated,
    /// `complete`: the whole file was shown. `changed`: it differs from then.
    Read {
        complete: bool,
        changed: bool,
    },
}

/// `bytes` are the file's current raw bytes.
pub(crate) fn prior(ctx: &ToolContext<'_>, path: &Path, meta: &std::fs::Metadata, bytes: &[u8]) -> Prior {
    let Some(stamp) = ctx.extensions.get_or_default::<FileStates>().get(path) else {
        return Prior::Unread;
    };
    if ctx.extensions.get_or_default::<TruncatedViews>().contains(path) {
        return Prior::Truncated;
    }
    let changed = mtime_ms(meta) > stamp.modified_ms && content_hash(bytes) != stamp.hash;
    Prior::Read { complete: !stamp.partial, changed }
}
