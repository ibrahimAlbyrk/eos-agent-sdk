//! Where a run's files go, in Claude Code's layout under one session
//! directory (`<root>/<session id>/`), and the script files a run may load.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use eos_agent_core::Workspace;

use crate::meta::MAX_SCRIPT;

/// One session's run files.
#[derive(Debug, Clone)]
pub struct SessionDir {
    dir: PathBuf,
}

impl SessionDir {
    pub fn new(root: &Path, session: &str) -> Self {
        Self { dir: root.join(session) }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn scripts(&self) -> PathBuf {
        self.dir.join("workflows").join("scripts")
    }

    /// `<session>/workflows/scripts/<slug(name)>-<runId>.js`.
    pub fn script(&self, name: &str, run_id: &str) -> PathBuf {
        self.scripts().join(format!("{}-{run_id}.js", slug(name)))
    }

    /// The run's transcript directory (its journal lives there).
    pub fn transcripts(&self, run_id: &str) -> PathBuf {
        self.dir.join("subagents").join("workflows").join(run_id)
    }

    /// The run record, written when the run ends.
    pub fn record(&self, run_id: &str) -> PathBuf {
        self.dir.join("workflows").join(format!("{run_id}.json"))
    }

    pub fn output(&self, task_id: &str) -> PathBuf {
        self.dir.join("tasks").join(format!("{task_id}.output"))
    }
}

/// Claude Code's `M_e`: lowercase, runs of other characters to `-`.
pub(crate) fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() { "workflow".into() } else { trimmed.to_owned() }
}

/// Writes a file only its owner can read (0600), in a 0700 directory.
pub(crate) fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut file, text.as_bytes())
}

/// Claude Code's refusal of a `scriptPath` it may not read.
pub(crate) fn not_readable(path: &str) -> String {
    format!(
        "scriptPath must be a script path this tool returned, or a file you can already read (the working directory \
         or a directory you have added): {path}\nA session that cannot call Read cannot load any scriptPath. Pass the \
         whole script inline through Workflow's `script` input instead."
    )
}

/// Where scripts may be loaded from: the agent's roots and the session's
/// own script folder.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Readable<'a> {
    pub(crate) workspace: Option<&'a Workspace>,
    pub(crate) session: &'a SessionDir,
}

impl Readable<'_> {
    fn resolve(&self, path: &str) -> PathBuf {
        match self.workspace {
            Some(ws) => ws.resolve(Path::new(path)),
            None => PathBuf::from(path),
        }
    }

    fn allows(&self, path: &Path) -> bool {
        path.starts_with(self.session.scripts()) || self.workspace.is_some_and(|ws| ws.readable_unasked(path))
    }

    /// Claude Code's `Aqr`: `None` when `path` may be loaded.
    pub(crate) fn check(&self, path: &str) -> Option<String> {
        if path.starts_with("\\\\") || path.starts_with("//") {
            return Some(not_readable(path));
        }
        (!self.allows(&self.resolve(path))).then(|| not_readable(path))
    }

    /// Claude Code's `kQn`: a regular file, not through a link, with one
    /// name, within the size cap; CRLF line ends normalized.
    pub(crate) fn load(&self, path: &str) -> Result<(String, PathBuf), String> {
        if let Some(e) = self.check(path) {
            return Err(e);
        }
        let full = self.resolve(path);
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&full)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("Workflow script file not found: {path}"));
            }
            Err(_) => return Err(format!("Failed to read workflow script file {path}")),
        };
        let meta = file.metadata().map_err(|_| format!("Failed to read workflow script file {path}"))?;
        if meta.ino() == 0 || meta.nlink() > 1 {
            return Err(not_readable(path));
        }
        let real = std::fs::canonicalize(&full).map_err(|_| not_readable(path))?;
        if !self.allows(&real) && !self.allows(&full) {
            return Err(not_readable(path));
        }
        if !meta.is_file() {
            return Err(format!("Workflow script file {path} is not a regular file"));
        }
        if meta.len() > MAX_SCRIPT as u64 {
            return Err(format!("Workflow script file {path} exceeds {MAX_SCRIPT} bytes"));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        file.read_to_end(&mut bytes).map_err(|_| format!("Failed to read workflow script file {path}"))?;
        Ok((normalize(&String::from_utf8_lossy(&bytes)), real))
    }
}

/// CRLF line ends to LF (Claude Code's `HLe`).
pub(crate) fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Claude Code's `TT`: no control characters an approval prompt cannot show
/// (tab, line feed and a CRLF's carriage return are fine).
pub(crate) fn printable(text: &str) -> bool {
    let b = text.as_bytes();
    b.iter().enumerate().all(|(i, &c)| match c {
        b'\t' | b'\n' => true,
        b'\r' => b.get(i + 1) == Some(&b'\n'),
        0..=0x1f | 0x7f => false,
        _ => true,
    }) && !text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_slugs() {
        let s = SessionDir::new(Path::new("/data"), "sess");
        assert_eq!(
            s.script("Review Changes!", "wf_1"),
            Path::new("/data/sess/workflows/scripts/review-changes-wf_1.js")
        );
        assert_eq!(s.transcripts("wf_1"), Path::new("/data/sess/subagents/workflows/wf_1"));
        assert_eq!(s.record("wf_1"), Path::new("/data/sess/workflows/wf_1.json"));
        assert_eq!(slug("--"), "workflow");
        assert!(printable("a\tb\r\nc") && !printable("a\u{1b}[31m") && !printable("a\rb"));
    }

    #[test]
    fn loads_only_readable_regular_files() {
        let root = std::env::temp_dir().join(format!("eos-wf-store-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).expect("dir");
        std::fs::write(cwd.join("w.js"), "export const meta = {}\r\nx").expect("write");
        let outside = root.join("outside.js");
        std::fs::write(&outside, "x").expect("write");
        std::os::unix::fs::symlink(&outside, cwd.join("link.js")).expect("link");
        let ws = Workspace::new(cwd.clone(), vec![], root.join("scratch"));
        let session = SessionDir::new(&root.join("data"), "s");
        let readable = Readable { workspace: Some(&ws), session: &session };
        let (text, path) = readable.load("w.js").expect("loads");
        assert_eq!(text, "export const meta = {}\nx");
        assert!(path.ends_with("proj/w.js"));
        assert_eq!(
            readable.load(outside.to_str().expect("utf8")).expect_err("outside"),
            not_readable(outside.to_str().expect("utf8"))
        );
        assert!(readable.load("link.js").is_err());
        assert_eq!(readable.load("missing.js").expect_err("missing"), "Workflow script file not found: missing.js");
        let own = session.script("demo", "wf_abc123");
        write_private(&own, "s").expect("write");
        assert_eq!(readable.load(own.to_str().expect("utf8")).expect("own script").0, "s");
        std::fs::remove_dir_all(root).ok();
    }
}
