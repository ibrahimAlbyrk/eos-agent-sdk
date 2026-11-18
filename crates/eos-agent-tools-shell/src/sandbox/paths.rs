//! The paths a sandbox profile names: the writable defaults, the protected
//! entries as rules, and settings paths resolved the way Seatbelt sees them.

use std::path::{Path, PathBuf};

use super::SandboxConfig;
use super::profile::{Rule, regex_escape};
pub(crate) use super::protect::Protected;

/// The devices every sandboxed command may write (sandbox-runtime's `NI`).
pub(crate) const WRITABLE_DEVICES: [&str; 6] =
    ["/dev/stdout", "/dev/stderr", "/dev/null", "/dev/tty", "/dev/dtracehelper", "/dev/autofs_nowait"];

/// Writable for every sandboxed command (sandbox-runtime's `NI`, as `RI`
/// filters it): its fixed devices and folders, and the home folder's
/// `.npm/_logs` and `.claude/debug` unless a `denyRead` entry covers them
/// (and no `allowRead` path inside that entry holds them again) or a
/// `deny` credential file does.
pub(crate) fn default_writable(config: &SandboxConfig, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> =
        WRITABLE_DEVICES.iter().chain(&["/tmp/claude", "/private/tmp/claude"]).map(PathBuf::from).collect();
    let Some(home) = home else { return out };
    let fs = &config.filesystem;
    let deny: Vec<Matcher> = fs.deny_read.iter().flatten().filter_map(|e| Matcher::new(e, cwd, Some(home))).collect();
    let allow: Vec<PathBuf> = fs
        .allow_read
        .iter()
        .flatten()
        .filter(|e| !is_glob(e))
        .filter_map(|e| expand(e, cwd, Some(home)))
        .map(|p| real(&p))
        .collect();
    let credential: Vec<Matcher> = config
        .credentials
        .iter()
        .flat_map(|c| c.deny_mode_files())
        .filter_map(|e| Matcher::new(&e, cwd, Some(home)))
        .collect();
    let homes = {
        let real_home = real(home);
        if real_home == home { vec![home.to_path_buf()] } else { vec![home.to_path_buf(), real_home] }
    };
    for name in [".npm/_logs", ".claude/debug"] {
        let spelled: Vec<PathBuf> = homes.iter().map(|h| h.join(name)).collect();
        let denied = deny.iter().any(|d| {
            spelled.iter().any(|p| d.matches(p))
                && !allow.iter().any(|a| d.matches(a) && spelled.iter().any(|p| p.starts_with(a)))
        });
        let credential = credential.iter().any(|d| spelled.iter().any(|p| d.matches(p)));
        if !denied && !credential {
            out.push(home.join(name));
        }
    }
    out
}

/// A read-deny entry as sandbox-runtime tests a path against it (`ae`): a
/// glob as a regex over the real path, else the real path and what is below.
enum Matcher {
    Glob(regex::Regex),
    Under(PathBuf),
}

impl Matcher {
    fn new(entry: &str, cwd: &Path, home: Option<&Path>) -> Option<Self> {
        let path = expand(entry, cwd, home)?;
        if is_glob(entry) {
            regex::Regex::new(&rust_regex(&glob_regex(&real(&path)))).ok().map(Self::Glob)
        } else {
            Some(Self::Under(real(&path)))
        }
    }

    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Glob(re) => re.is_match(&path.to_string_lossy()),
            Self::Under(root) => path.starts_with(root),
        }
    }
}

impl Protected {
    /// As the profile states it, with `root` already resolved by the caller.
    pub(crate) fn rule(&self) -> Rule {
        match self {
            Self::Path(p) => Rule::Subpath(p.clone()),
            Self::Below { root, tail } => {
                let tail = match tail.strip_suffix("/**") {
                    Some(dir) => format!("{}/.*", regex_escape(dir)),
                    None => regex_escape(tail),
                };
                Rule::Regex(format!("^{}/(.*/)?{tail}(/.*)?$", regex_escape(&root.to_string_lossy())))
            }
            Self::Glob(glob) => Rule::Regex(glob_regex(Path::new(glob))),
        }
    }

    /// As the prompt shows it.
    pub(crate) fn shown(&self) -> String {
        match self {
            Self::Path(p) => p.to_string_lossy().into_owned(),
            Self::Below { root, tail } => format!("{}/**/{tail}", root.display()),
            Self::Glob(glob) => glob.clone(),
        }
    }
}

/// The profile's rules for `entries`, with the names protected at any depth
/// under one root joined into one regex per kind (Seatbelt compiles every
/// regex for each command; Claude Code groups them too).
pub(crate) fn rules(entries: &[Protected]) -> Vec<Rule> {
    let mut out = Vec::new();
    let mut groups: Vec<(PathBuf, bool, Vec<String>)> = Vec::new();
    for entry in entries {
        match entry {
            Protected::Path(_) | Protected::Glob(_) => out.push(entry.rule()),
            Protected::Below { root, tail } => {
                let (dir, name) = match tail.strip_suffix("/**") {
                    Some(d) => (true, d),
                    None => (false, tail.as_str()),
                };
                match groups.iter_mut().find(|(r, d, _)| r == root && *d == dir) {
                    Some((_, _, names)) => names.push(regex_escape(name)),
                    None => groups.push((root.clone(), dir, vec![regex_escape(name)])),
                }
            }
        }
    }
    for (root, dir, names) in groups {
        let inner = if dir { "/.*" } else { "" };
        let root = regex_escape(&root.to_string_lossy());
        out.push(Rule::Regex(format!("^{root}/(.*/)?({}){inner}(/.*)?$", names.join("|"))));
    }
    out
}

/// A settings path: `~` and `~/…` from home, relative ones from `cwd`, a
/// trailing `/**` dropped (a directory means everything below it).
pub(crate) fn expand(path: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let path = path.strip_suffix("/**").unwrap_or(path);
    let expanded = match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        (Some(_), _) => return None,
        _ => cwd.join(path),
    };
    Some(expanded)
}

pub(crate) fn is_glob(path: &str) -> bool {
    path.strip_suffix("/**").unwrap_or(path).contains(['*', '?', '['])
}

/// A glob as the anchored regex sandbox-runtime writes for it (`SM`):
/// `.^$+{}()|\` escaped, `[…]` classes kept (an unclosed `[` escaped),
/// `**/` any directories, `**` anything, `*` and `?` within one name, and
/// everything below a match.
pub(crate) fn glob_regex(glob: &Path) -> String {
    let text = glob.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if ".^$+{}()|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    // The first `[` after the last `]` opens no class.
    let last_close = out.rfind(']');
    if let Some(open) = out.char_indices().find(|&(i, c)| c == '[' && last_close.is_none_or(|l| i > l)).map(|(i, _)| i)
    {
        out.insert(open, '\\');
    }
    let out = out
        .replace("**/", "\u{0}GS\u{0}")
        .replace("**", "\u{0}G\u{0}")
        .replace('*', "[^/]*")
        .replace('?', "[^/]")
        .replace("\u{0}GS\u{0}", "(.*/)?")
        .replace("\u{0}G\u{0}", ".*");
    format!("^{out}(/.*)?$")
}

/// [`glob_regex`]'s output for Rust's `regex`, which reads `[`, `&&`, `~~`
/// and `--` inside a class as set syntax: those are escaped there.
fn rust_regex(sbpl: &str) -> String {
    let mut out = String::with_capacity(sbpl.len() + 8);
    let mut in_class = false;
    let mut escaped = false;
    for c in sbpl.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' if in_class => {
                out.push_str("\\[");
                continue;
            }
            '[' => in_class = true,
            ']' if in_class => in_class = false,
            ']' => {
                out.push_str("\\]");
                continue;
            }
            '&' | '~' if in_class => out.push('\\'),
            _ => {}
        }
        out.push(c);
    }
    out
}

/// The real path, the way Seatbelt matches it: symlinks resolved, and for a
/// path that does not exist yet, its nearest existing parent resolved.
pub(crate) fn real(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut rest = Vec::new();
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_os_string());
        }
        if let Ok(real) = std::fs::canonicalize(parent) {
            return rest.iter().rev().fold(real, |p, n| p.join(n));
        }
        cur = parent;
    }
    path.to_path_buf()
}

/// The path as given and as resolved, when they differ (both spellings go
/// into the profile, as Claude Code emits them).
pub(crate) fn spellings(path: &Path) -> Vec<PathBuf> {
    let resolved = real(path);
    if resolved == path { vec![resolved] } else { vec![path.to_path_buf(), resolved] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_entries_become_rules() {
        let below = Protected::Below { root: "/w/p.x".into(), tail: ".git/hooks/**".into() };
        assert_eq!(below.rule(), Rule::Regex(r"^/w/p\.x/(.*/)?\.git/hooks/.*(/.*)?$".into()));
        assert_eq!(below.shown(), "/w/p.x/**/.git/hooks/**");
        let file = Protected::Below { root: "/w".into(), tail: ".zshrc".into() };
        assert_eq!(file.rule(), Rule::Regex(r"^/w/(.*/)?\.zshrc(/.*)?$".into()));
        let dirs = super::super::SandboxDirs { home: Some("/Users/u".into()), ..Default::default() };
        let set = super::super::protect::fence(Path::new("/w"), &["/w".into()], &dirs).write;
        let grouped = rules(&set);
        let regexes: Vec<&Rule> = grouped.iter().filter(|r| matches!(r, Rule::Regex(_))).collect();
        // Six nested `config.lock` globs, then one regex for files and one for directories.
        assert_eq!(regexes.len(), 8);
        assert_eq!(
            regexes[0],
            &Rule::Regex(r"^/w/(.*/)?\.[gG][iI][tT]/[cC][oO][nN][fF][iI][gG]\.[lL][oO][cC][kK](/.*)?$".into())
        );
        assert_eq!(
            regexes[6],
            &Rule::Regex(
                r"^/w/(.*/)?(\.gitconfig|\.gitmodules|\.bashrc|\.bash_profile|\.zshrc|\.zprofile|\.profile|\.ripgreprc|\.mcp\.json|\.git/config)(/.*)?$"
                    .into()
            )
        );
        assert!(set.contains(&Protected::Path("/w/.git/config".into())));
        assert!(set.contains(&Protected::Path("/w/.claude/settings.local.json".into())));
        assert!(set.contains(&Protected::Path("/Users/u/.claude/settings.json".into())));
    }

    #[test]
    fn settings_paths() {
        let home = Some(Path::new("/Users/u"));
        assert_eq!(expand("~/.ssh", Path::new("/w"), home), Some("/Users/u/.ssh".into()));
        assert_eq!(expand("build/**", Path::new("/w"), home), Some("/w/build".into()));
        assert_eq!(expand("/abs", Path::new("/w"), home), Some("/abs".into()));
        assert!(is_glob("src/*.rs") && !is_glob("src/**") && !is_glob("/a/b"));
        assert_eq!(glob_regex(Path::new("/w/**/*.pem")), r"^/w/(.*/)?[^/]*\.pem(/.*)?$");
        // Classes stay classes, an unclosed `[` is a character, as sandbox-runtime writes them.
        assert_eq!(glob_regex(Path::new("/w/[ab]?.[")), r"^/w/[ab][^/]\.\[(/.*)?$");
        assert_eq!(glob_regex(Path::new("/w/[[]x]")), r"^/w/[[]x](/.*)?$");
        let rust = regex::Regex::new(&rust_regex(&glob_regex(Path::new("/w/[[]x]")))).expect("compiles");
        assert!(rust.is_match("/w/[x]/y") && !rust.is_match("/w/ax]"));
    }

    #[test]
    fn resolves_like_seatbelt() {
        let tmp = std::env::temp_dir();
        let real_tmp = std::fs::canonicalize(&tmp).expect("temp dir");
        assert_eq!(real(&tmp.join("eos-no-such-dir/x")), real_tmp.join("eos-no-such-dir/x"));
        if real_tmp != tmp {
            assert_eq!(spellings(&tmp), vec![tmp.clone(), real_tmp]);
        }
    }
}
