//! Per-agent state that several tool families share, kept in the agent's
//! [`Extensions`](crate::Extensions).

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

/// Where an agent works: its current directory and its working directories
/// (the roots).
///
/// Tools read and write anywhere the process can, and the permission gate
/// decides each call, except that a tool never writes a protected path
/// outside the roots, and a confined agent (one isolated in its own
/// worktree) writes only inside its roots and the engine's writable files.
/// The agent's own scratch folder counts as inside, as Claude Code lets an
/// agent write its scratchpad without asking.
#[derive(Debug)]
pub struct Workspace {
    cwd: RwLock<PathBuf>,
    roots: Vec<PathBuf>,
    /// Scratch space for this agent's spilled outputs and background task logs.
    scratch: PathBuf,
    internal: InternalPaths,
    confined: bool,
    protected: Vec<PathBuf>,
}

/// A path a tool call would create or change, as the permission gate sees
/// it ([`ToolCall::writes`](crate::ToolCall::writes)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    /// Absolute, `~` expanded, `.` and `..` resolved; symlinks as written.
    pub path: PathBuf,
    /// Inside the agent's working directories (or one of the engine's
    /// writable files), by the path as written and by where its symlinks
    /// lead. `false`: the write lands outside them.
    pub inside: bool,
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
        Self {
            cwd: RwLock::new(normalize(&cwd)),
            roots,
            scratch,
            internal: InternalPaths::default(),
            confined: false,
            protected: Vec::new(),
        }
    }

    /// Confines writes to the roots and the engine's writable files: for an
    /// agent isolated in its own worktree, so parallel agents cannot write
    /// over each other's files whatever the permission gate allows.
    #[must_use]
    pub fn confined(mut self, confined: bool) -> Self {
        self.confined = confined;
        self
    }

    /// Paths no tool writes outside the roots and the agent's own scratch
    /// folder, whatever the permission gate allows (the engine's own data:
    /// transcripts, other agents' scratch, sign-in stores).
    #[must_use]
    pub fn with_protected(mut self, protected: Vec<PathBuf>) -> Self {
        self.protected = protected.iter().map(|p| normalize(p)).collect();
        self
    }

    /// Writes stay inside the roots ([`confined`](Self::confined)).
    pub fn is_confined(&self) -> bool {
        self.confined
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

    /// [`resolve`](Self::resolve) for a path as a model writes it: a leading
    /// `~` is the home folder.
    pub fn resolve_user(&self, raw: &str) -> PathBuf {
        let expanded = match raw.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(rest.trim_start_matches('/')),
                None => PathBuf::from(raw),
            },
            _ => PathBuf::from(raw),
        };
        self.resolve(&expanded)
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
            || in_writable(&path, &self.internal.writable)
    }

    /// `path` as the permission gate sees a write to it.
    pub fn write_target(&self, path: &Path) -> WriteTarget {
        let path = self.resolve(path);
        WriteTarget { inside: self.writes_inside(&path), path }
    }

    /// Inside the roots, the agent's scratch folder or one of the engine's
    /// writable files, both as written and where the path's symlinks lead
    /// (a link inside the roots to a folder outside them is outside). Reads
    /// the filesystem.
    pub fn writes_inside(&self, path: &Path) -> bool {
        let path = self.resolve(path);
        let scratch = normalize(&self.scratch);
        if !(self.contains(&path) || path.starts_with(&scratch) || in_writable(&path, &self.internal.writable)) {
            return false;
        }
        let real = real_path(&path);
        let roots: Vec<PathBuf> = self.roots.iter().chain([&scratch]).map(|r| real_path(r)).collect();
        let writable: Vec<(PathBuf, String)> =
            self.internal.writable.iter().map(|(dir, end)| (real_path(dir), end.clone())).collect();
        roots.iter().any(|r| real.starts_with(r)) || in_writable(&real, &writable)
    }

    /// Why a tool may not write `path`, or `None` when it may (the
    /// permission gate has decided by then). A protected path outside the
    /// roots and the scratch folder is refused, and so is anything outside
    /// them for a confined agent.
    pub fn write_refusal(&self, path: &Path) -> Option<String> {
        let path = self.resolve(path);
        if self.writes_inside(&path) {
            return None;
        }
        if self.confined {
            return Some(format!("{} is outside this agent's working directories.", path.display()));
        }
        let real = real_path(&path);
        let protected = self.protected.iter().any(|p| path.starts_with(p) || real.starts_with(real_path(p)));
        protected.then(|| format!("{} is in the agent engine's own files, which tools do not write.", path.display()))
    }

    /// [`write_refusal`](Self::write_refusal) finds nothing against it.
    pub fn may_write(&self, path: &Path) -> bool {
        self.write_refusal(path).is_none()
    }
}

/// One of the engine's writable files: in `dir` (or below), with the name ending.
fn in_writable(path: &Path, writable: &[(PathBuf, String)]) -> bool {
    writable.iter().any(|(dir, end)| {
        path.starts_with(dir) && path != dir.as_path() && path.to_string_lossy().ends_with(end.as_str())
    })
}

/// Where `path` leads: its nearest existing ancestor with symlinks resolved,
/// the rest as written (a dangling link at the end is replaced, not followed).
fn real_path(path: &Path) -> PathBuf {
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            return match path.strip_prefix(&probe) {
                Ok(rest) if !rest.as_os_str().is_empty() => real.join(rest),
                _ => real,
            };
        }
        if !probe.pop() {
            return path.to_path_buf();
        }
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
        assert!(!ws.writes_inside(Path::new("/data/s1/subagents/workflows/wf_1/journal.jsonl")));
        assert!(ws.writes_inside(Path::new("/data/s1/workflows/scripts/demo-wf_1.js")));
        assert!(!ws.writes_inside(Path::new("/data/s1/workflows/scripts/demo.txt")));
        assert!(!ws.writes_inside(Path::new("/data/s1/workflows/scripts/../x.js")));
        assert!(ws.writes_inside(Path::new("/data/scratch/a1/t.txt")));
        assert!(!ws.writes_inside(Path::new("/data/scratch/a2/t.txt")), "another agent's scratch");
        assert!(ws.writes_inside(Path::new("/work/proj/x")));
    }

    #[test]
    fn writes_are_confined_only_when_isolated() {
        let free = Workspace::new("/work/proj".into(), vec![], "/tmp/x".into());
        assert!(free.may_write(Path::new("/work/other/a.txt")));
        assert_eq!(
            free.write_target(Path::new("../other/a.txt")),
            WriteTarget { path: "/work/other/a.txt".into(), inside: false }
        );
        assert_eq!(
            free.write_target(Path::new("a.txt")),
            WriteTarget { path: "/work/proj/a.txt".into(), inside: true }
        );
        let confined = Workspace::new("/work/proj".into(), vec![], "/tmp/x".into()).confined(true);
        assert_eq!(
            confined.write_refusal(Path::new("/work/other/a.txt")).as_deref(),
            Some("/work/other/a.txt is outside this agent's working directories.")
        );
        assert!(confined.may_write(Path::new("/work/proj/src/a.rs")));
        assert!(confined.may_write(Path::new("/tmp/x/a.txt")), "its own scratch");
    }

    #[test]
    fn protected_paths_stay_unwritten_outside_the_roots() {
        let ws = Workspace::new("/data/engine/proj".into(), vec![], "/data/engine/scratch/a1".into())
            .with_protected(vec!["/data/engine".into()])
            .with_internal(InternalPaths {
                readable: vec![],
                writable: vec![("/data/engine/s1/workflows/scripts".into(), ".js".into())],
            });
        assert!(ws.write_refusal(Path::new("/data/engine/transcripts/a1.jsonl")).is_some());
        assert!(ws.may_write(Path::new("/data/engine/scratch/a1/t.txt")), "its own scratch");
        assert!(ws.write_refusal(Path::new("/data/engine/scratch/a2/t.txt")).is_some(), "another agent's scratch");
        assert!(ws.may_write(Path::new("/data/engine/s1/workflows/scripts/demo.js")));
        assert!(ws.may_write(Path::new("/data/engine/proj/src/a.rs")), "the roots win");
        assert!(ws.may_write(Path::new("/data/other/a.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_judged_by_where_it_leads() {
        let base = std::env::temp_dir().join(format!("eos-ws-link-{}", std::process::id()));
        let (proj, outside) = (base.join("proj"), base.join("outside"));
        std::fs::create_dir_all(&proj).expect("proj");
        std::fs::create_dir_all(&outside).expect("outside");
        let _ = std::fs::remove_file(proj.join("out"));
        std::os::unix::fs::symlink(&outside, proj.join("out")).expect("link");
        let ws = Workspace::new(proj.clone(), vec![], base.join("scratch"));
        assert!(ws.writes_inside(&proj.join("a.txt")));
        assert!(!ws.writes_inside(&proj.join("out/a.txt")), "a link inside to a folder outside is outside");
        let confined = Workspace::new(proj.clone(), vec![], base.join("scratch")).confined(true);
        assert!(!confined.may_write(&proj.join("out/a.txt")));
        let protected =
            Workspace::new(base.join("elsewhere"), vec![], base.join("scratch")).with_protected(vec![outside.clone()]);
        assert!(!protected.may_write(&proj.join("out/a.txt")), "protection follows the link");
        std::fs::write(outside.join("s.js"), "").expect("script");
        let scripts = Workspace::new(proj.clone(), vec![], base.join("scratch"))
            .with_internal(InternalPaths { readable: vec![], writable: vec![(outside.clone(), ".js".into())] });
        assert!(scripts.writes_inside(&outside.join("s.js")), "an existing writable file keeps its name");
        let _ = std::fs::remove_dir_all(&base);
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
