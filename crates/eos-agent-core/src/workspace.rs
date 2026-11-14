//! Per-agent state that several tool families share, kept in the agent's
//! [`Extensions`](crate::Extensions).

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

/// Where an agent works: its current directory and the roots it may touch.
#[derive(Debug)]
pub struct Workspace {
    cwd: RwLock<PathBuf>,
    roots: Vec<PathBuf>,
    /// Scratch space for this agent's spilled outputs and background task logs.
    scratch: PathBuf,
    internal: InternalPaths,
}

/// The engine's own files an agent may use outside its roots without asking,
/// as Claude Code allows its session files ("Project directory files are
/// allowed for reading", "Workflow script files for current session are
/// allowed for writing"). The scratch folder is one too ("Tool result files
/// are allowed for reading").
#[derive(Debug, Clone, Default)]
pub struct InternalPaths {
    /// Readable anywhere below.
    pub readable: Vec<PathBuf>,
    /// Writable files: in this directory (or below), with this name ending.
    pub writable: Vec<(PathBuf, String)>,
}

impl Workspace {
    /// `cwd` is also the first root.
    pub fn new(cwd: PathBuf, extra_roots: Vec<PathBuf>, scratch: PathBuf) -> Self {
        let mut roots = vec![normalize(&cwd)];
        roots.extend(extra_roots.iter().map(|r| normalize(r)));
        Self { cwd: RwLock::new(normalize(&cwd)), roots, scratch, internal: InternalPaths::default() }
    }

    /// Adds the engine's files the agent may read or write outside its roots.
    #[must_use]
    pub fn with_internal(mut self, internal: InternalPaths) -> Self {
        self.internal = InternalPaths {
            readable: internal.readable.iter().map(|p| normalize(p)).collect(),
            writable: internal.writable.iter().map(|(p, end)| (normalize(p), end.clone())).collect(),
        };
        self
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Moves the current directory, only within the roots. Returns whether it moved.
    pub fn set_cwd(&self, dir: &Path) -> bool {
        let dir = self.resolve(dir);
        if !self.contains(&dir) {
            return false;
        }
        *self.cwd.write().unwrap_or_else(PoisonError::into_inner) = dir;
        true
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Absolute, `.`/`..`-free form of `path`, relative ones taken from the cwd.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { normalize(path) } else { normalize(&self.cwd().join(path)) }
    }

    pub fn contains(&self, path: &Path) -> bool {
        let path = self.resolve(path);
        self.roots.iter().any(|r| path.starts_with(r))
    }

    /// Inside the roots, the scratch folder or the engine's readable files:
    /// what Claude Code reads without asking, the only place a Workflow
    /// `scriptPath` loads from. The tools that only read take any path; the
    /// permission gate decides those.
    pub fn readable_unasked(&self, path: &Path) -> bool {
        let path = self.resolve(path);
        self.contains(&path)
            || path.starts_with(normalize(&self.scratch))
            || self.internal.readable.iter().any(|r| path.starts_with(r))
            || self.may_write_internal(&path)
    }

    /// Inside the roots or one of the engine's writable files.
    pub fn may_write(&self, path: &Path) -> bool {
        let path = self.resolve(path);
        self.contains(&path) || self.may_write_internal(&path)
    }

    fn may_write_internal(&self, path: &Path) -> bool {
        self.internal.writable.iter().any(|(dir, end)| {
            path.starts_with(dir) && path != dir.as_path() && path.to_string_lossy().ends_with(end.as_str())
        })
    }
}

/// Lexical normalization (no filesystem access, symlinks untouched).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The agent's id, for tools that act for it outside a call (rebuilding
/// state from a resumed history). The engine sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdent(pub String);

/// What the agent last saw of a file. No content is kept: the stamp tells
/// whether the file changed since, and edits re-check the current content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub modified_ms: u64,
    pub len: u64,
    /// Content hash at read time (FNV-1a), to tell a touched file from a changed one.
    pub hash: u64,
    /// Only part of the file was shown (offset/limit or a size cap).
    pub partial: bool,
}

/// The files an agent has read or written, most recent kept, bounded.
#[derive(Debug)]
pub struct FileStates {
    inner: Mutex<(HashMap<PathBuf, FileStamp>, VecDeque<PathBuf>)>,
    capacity: usize,
}

impl Default for FileStates {
    fn default() -> Self {
        Self::with_capacity(256)
    }
}

impl FileStates {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { inner: Mutex::new((HashMap::new(), VecDeque::new())), capacity: capacity.max(1) }
    }

    pub fn record(&self, path: PathBuf, stamp: FileStamp) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (map, order) = &mut *guard;
        if map.insert(path.clone(), stamp).is_some() {
            order.retain(|p| p != &path);
        }
        order.push_back(path);
        while order.len() > self.capacity {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<FileStamp> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).0.get(path).copied()
    }

    pub fn forget(&self, path: &Path) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.0.remove(path);
        guard.1.retain(|p| p != path);
    }

    /// Every tracked path with its stamp (for change reminders).
    pub fn snapshot(&self) -> Vec<(PathBuf, FileStamp)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).0.iter().map(|(p, s)| (p.clone(), *s)).collect()
    }
}

/// FNV-1a: fast, dependency-free, good enough to tell two file versions apart.
pub fn content_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// Notes for the model, delivered with the next request as a user message
/// (background tasks finishing, files changing, …). Producers write the text
/// already wrapped as they want it shown.
#[derive(Debug, Default)]
pub struct Reminders {
    pending: Mutex<Vec<String>>,
}

impl Reminders {
    pub fn push(&self, text: impl Into<String>) {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).push(text.into());
    }

    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_stays_inside_roots() {
        let ws = Workspace::new("/work/proj".into(), vec!["/data".into()], "/tmp/x".into());
        assert!(ws.set_cwd(Path::new("src")));
        assert_eq!(ws.cwd(), PathBuf::from("/work/proj/src"));
        assert_eq!(ws.resolve(Path::new("../README.md")), PathBuf::from("/work/proj/README.md"));
        assert!(!ws.set_cwd(Path::new("/etc")));
        assert!(ws.set_cwd(Path::new("/data/sets")));
        assert!(!ws.contains(Path::new("/work/other")));
    }

    #[test]
    fn internal_files_outside_the_roots() {
        let ws = Workspace::new("/work/proj".into(), vec![], "/data/scratch/a1".into()).with_internal(InternalPaths {
            readable: vec!["/data/s1".into()],
            writable: vec![("/data/s1/workflows/scripts".into(), ".js".into())],
        });
        assert!(ws.readable_unasked(Path::new("/data/scratch/a1/tool-results/t.txt")));
        assert!(ws.readable_unasked(Path::new("/data/s1/subagents/workflows/wf_1/journal.jsonl")));
        assert!(!ws.readable_unasked(Path::new("/data/s2/x")));
        assert!(!ws.may_write(Path::new("/data/s1/subagents/workflows/wf_1/journal.jsonl")));
        assert!(ws.may_write(Path::new("/data/s1/workflows/scripts/demo-wf_1.js")));
        assert!(!ws.may_write(Path::new("/data/s1/workflows/scripts/demo.txt")));
        assert!(!ws.may_write(Path::new("/data/s1/workflows/scripts/../x.js")));
        assert!(!ws.may_write(Path::new("/data/scratch/a1/t.txt")));
        assert!(ws.may_write(Path::new("/work/proj/x")));
    }

    #[test]
    fn file_states_are_bounded() {
        let fs = FileStates::with_capacity(2);
        let stamp = FileStamp { modified_ms: 1, len: 1, hash: 1, partial: false };
        fs.record("/a".into(), stamp);
        fs.record("/b".into(), stamp);
        fs.record("/a".into(), stamp); // refresh /a
        fs.record("/c".into(), stamp); // evicts /b
        assert!(fs.get(Path::new("/a")).is_some());
        assert!(fs.get(Path::new("/b")).is_none());
        assert_eq!(fs.snapshot().len(), 2);
    }
}
