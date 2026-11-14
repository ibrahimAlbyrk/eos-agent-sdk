//! CLAUDE.md discovery in Claude Code's order: managed, user, then every
//! directory from the filesystem root down to the working directory, each
//! file followed by the files it imports.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ClaudePaths;
use crate::markdown;

/// Imports nest at most this deep (the file itself is depth 0).
const MAX_DEPTH: usize = 5;
/// Larger files are skipped, as Claude Code does.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// An import must be a text file; one without an extension is accepted.
#[rustfmt::skip]
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "txt", "text", "json", "yaml", "yml", "toml", "xml", "csv", "html", "htm", "css", "scss", "sass", "less",
    "js", "ts", "tsx", "jsx", "mjs", "cjs", "mts", "cts", "py", "pyi", "pyw", "rb", "erb", "rake", "go", "rs", "java",
    "kt", "kts", "scala", "c", "cpp", "cc", "cxx", "h", "hpp", "hxx", "cs", "swift", "sh", "bash", "zsh", "fish",
    "ps1", "bat", "cmd", "env", "ini", "cfg", "conf", "config", "properties", "sql", "graphql", "gql", "proto", "vue",
    "svelte", "astro", "ejs", "hbs", "pug", "jade", "php", "pl", "pm", "lua", "r", "dart", "ex", "exs", "erl", "hrl",
    "clj", "cljs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// Organization policy (the managed folder).
    Managed,
    /// `~/.claude/CLAUDE.md` and `~/.claude/rules`.
    User,
    /// `CLAUDE.md`, `.claude/CLAUDE.md` and `.claude/rules` in the project's directories.
    Project,
    /// `CLAUDE.local.md`: the user's private project notes.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMdFile {
    pub path: PathBuf,
    pub scope: MemoryScope,
    /// Frontmatter and block-level HTML comments removed.
    pub content: String,
    /// The file whose `@path` brought this one in.
    pub imported_from: Option<PathBuf>,
}

/// Every instruction file Claude Code would load for a session in `cwd`.
/// `allow_external_imports` lets project files import files outside `cwd`
/// (Claude Code asks the user first; see
/// [`GlobalConfig::allows_external_imports`](crate::GlobalConfig::allows_external_imports)).
/// Blocking I/O: a few `stat`s per directory level plus the files found.
pub fn discover_claude_md(paths: &ClaudePaths, cwd: &Path, allow_external_imports: bool) -> Vec<ClaudeMdFile> {
    let mut loader = Loader { home: &paths.home, cwd, allow_external_imports, seen: HashSet::new(), out: Vec::new() };
    loader.load(&paths.managed_dir.join("CLAUDE.md"), MemoryScope::Managed, 0, None);
    loader.rules(&paths.managed_dir.join(".claude/rules"), MemoryScope::Managed);
    loader.load(&paths.config_dir.join("CLAUDE.md"), MemoryScope::User, 0, None);
    loader.rules(&paths.config_dir.join("rules"), MemoryScope::User);

    let mut dirs: Vec<&Path> = cwd.ancestors().filter(|d| d.parent().is_some()).collect();
    dirs.reverse();
    for dir in dirs {
        loader.load(&dir.join("CLAUDE.md"), MemoryScope::Project, 0, None);
        loader.load(&dir.join(".claude/CLAUDE.md"), MemoryScope::Project, 0, None);
        loader.rules(&dir.join(".claude/rules"), MemoryScope::Project);
        loader.load(&dir.join("CLAUDE.local.md"), MemoryScope::Local, 0, None);
    }
    loader.out
}

struct Loader<'a> {
    home: &'a Path,
    cwd: &'a Path,
    allow_external_imports: bool,
    seen: HashSet<PathBuf>,
    out: Vec<ClaudeMdFile>,
}

impl Loader<'_> {
    fn load(&mut self, path: &Path, scope: MemoryScope, depth: usize, parent: Option<&Path>) {
        if depth >= MAX_DEPTH {
            return;
        }
        if depth > 0 && !self.may_import(path, scope) {
            return;
        }
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.seen.contains(&key) {
            return;
        }
        let Some(raw) = read_text(path) else { return };
        self.seen.insert(key);
        let (_, body) = markdown::split_frontmatter(&raw);
        let content = markdown::strip_block_comments(body);
        if content.trim().is_empty() {
            return;
        }
        let imports = markdown::imports(&content);
        self.out.push(ClaudeMdFile {
            path: path.to_path_buf(),
            scope,
            content,
            imported_from: parent.map(Path::to_path_buf),
        });
        let base = path.parent().unwrap_or(Path::new("/"));
        for target in imports {
            let resolved = self.resolve(&target, base);
            self.load(&resolved, scope, depth + 1, Some(path));
        }
    }

    /// Loads the `.md` files under a rules folder, sorted, skipping rules
    /// that apply only to some paths.
    fn rules(&mut self, dir: &Path, scope: MemoryScope) {
        let mut files = Vec::new();
        collect_md(dir, &mut files, 0);
        files.sort();
        for file in files {
            let conditional = read_text(&file)
                .is_some_and(|t| markdown::split_frontmatter(&t).0.is_some_and(markdown::is_conditional));
            if !conditional {
                self.load(&file, scope, 0, None);
            }
        }
    }

    fn may_import(&self, path: &Path, scope: MemoryScope) -> bool {
        let text = path.extension().and_then(|e| e.to_str()).is_none_or(|ext| TEXT_EXTENSIONS.contains(&ext));
        let inside = path.starts_with(self.cwd);
        text && (inside || scope == MemoryScope::User || self.allow_external_imports)
    }

    fn resolve(&self, target: &str, base: &Path) -> PathBuf {
        let joined = match target.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => base.join(target),
        };
        normalize(&joined)
    }
}

fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Rules folders are walked without following directory links, so a link
/// loop cannot recurse forever.
fn collect_md(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && depth < 16 {
            collect_md(&path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
