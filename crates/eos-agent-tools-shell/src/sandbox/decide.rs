//! Whether a command runs sandboxed (Claude Code's `shouldUseSandbox`), and
//! whether a sandboxed one may skip the permission gate.

use std::path::{Component, Path, PathBuf};

use super::SandboxConfig;
use crate::lexer::{Simple, lex};

/// Claude Code's order: sandbox off → no; `dangerouslyDisableSandbox` while
/// unsandboxed commands are allowed → no; every part of the command matches
/// an `excludedCommands` entry (and unsandboxed commands are allowed) → no;
/// else yes.
pub fn should_sandbox(config: &SandboxConfig, command: &str, disable_sandbox: bool) -> bool {
    if !config.enabled() {
        return false;
    }
    let unsandboxed_ok = config.allow_unsandboxed_commands();
    if disable_sandbox && unsandboxed_ok {
        return false;
    }
    if command.trim().is_empty() {
        return !unsandboxed_ok;
    }
    !(unsandboxed_ok && is_excluded(config.excluded_commands(), command))
}

/// Every simple command matches an entry. A `cd`, a command substitution or
/// a redirection other than a file-descriptor duplication keeps the call
/// sandboxed whatever the entries say.
fn is_excluded(patterns: &[String], command: &str) -> bool {
    if patterns.is_empty() || command.contains("$(") || command.contains('`') {
        return false;
    }
    let line = lex(command);
    if line.opaque || line.to_null || line.commands.is_empty() {
        return false;
    }
    if line.commands.iter().any(is_cd) {
        return false;
    }
    line.commands.iter().all(|cmd| {
        let words: Vec<&str> = cmd.words.iter().map(String::as_str).collect();
        let full = words.join(" ");
        let bare = words[env_prefix_len(&words)..].join(" ");
        patterns.iter().any(|p| matches_rule(p, &full) || matches_rule(p, &bare))
    })
}

fn is_cd(cmd: &Simple) -> bool {
    let words: Vec<&str> = cmd.words.iter().map(String::as_str).collect();
    let name = words.get(env_prefix_len(&words)).map(|w| w.rsplit('/').next().unwrap_or(w));
    matches!(name, Some("cd" | "pushd" | "popd" | "chdir"))
}

/// Leading `NAME=value` words.
fn env_prefix_len(words: &[&str]) -> usize {
    words.iter().take_while(|w| assignment(w).is_some()).count()
}

fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some((name, value))
}

/// Bash permission-rule syntax: `npm test:*` (prefix), `git *` (wildcard,
/// which also matches the bare `git`), else the exact command.
pub(crate) fn matches_rule(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return command == prefix || command.starts_with(&format!("{prefix} "));
    }
    if pattern.contains('*') {
        if let Some(prefix) = pattern.strip_suffix(" *")
            && !prefix.contains('*')
            && command == prefix
        {
            return true;
        }
        return wildcard(pattern.as_bytes(), command.as_bytes());
    }
    pattern == command
}

fn wildcard(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(at) = star {
            pi = at + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// Variables Claude Code lets a sandboxed command set without losing the
/// auto-allow (`L5e`).
const SAFE_ENV: &[&str] = &[
    "GOEXPERIMENT",
    "GOOS",
    "GOARCH",
    "CGO_ENABLED",
    "GO111MODULE",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "NODE_ENV",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
    "PYTEST_DEBUG",
    "ANTHROPIC_API_KEY",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TIME",
    "CHARSET",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "FORCE_COLOR",
    "TZ",
    "LS_COLORS",
    "LSCOLORS",
    "GREP_COLOR",
    "GREP_COLORS",
    "GCC_COLORS",
    "TIME_STYLE",
    "BLOCK_SIZE",
    "BLOCKSIZE",
    "COLUMNS",
    "LINES",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "CI",
    "DEBIAN_FRONTEND",
    "GIT_TERMINAL_PROMPT",
];

/// Top-level directories whose removal Claude Code never auto-allows.
const CRITICAL: &[&str] = &[
    "/",
    "/Applications",
    "/Library",
    "/System",
    "/Users",
    "/Volumes",
    "/bin",
    "/cores",
    "/dev",
    "/etc",
    "/home",
    "/opt",
    "/private",
    "/private/etc",
    "/private/tmp",
    "/private/var",
    "/root",
    "/sbin",
    "/tmp",
    "/usr",
    "/usr/bin",
    "/usr/lib",
    "/usr/local",
    "/usr/sbin",
    "/var",
];

/// Claude Code's exceptions to the auto-allow of a sandboxed command
/// (`T5e`): it goes to the permission gate instead when it sets a variable
/// that is not on the safe list (or to a path), redirects to `/dev/tcp` or
/// `/dev/udp`, combines `cd` with `rm`, or removes a critical directory, a
/// working directory or one of its parents, or a target it cannot resolve.
pub fn needs_permission_anyway(command: &str, cwd: &Path, roots: &[PathBuf], home: Option<&Path>) -> bool {
    if command.contains("/dev/tcp/") || command.contains("/dev/udp/") {
        return true;
    }
    let line = lex(command);
    let mut cd = false;
    let mut rm = false;
    for cmd in &line.commands {
        let words: Vec<&str> = cmd.words.iter().map(String::as_str).collect();
        let skip = env_prefix_len(&words);
        for word in &words[..skip] {
            if let Some((name, value)) = assignment(word)
                && (!SAFE_ENV.contains(&name) || path_like(value))
            {
                return true;
            }
        }
        let Some(name) = words.get(skip).map(|w| w.rsplit('/').next().unwrap_or(w)) else { continue };
        match name {
            "cd" | "pushd" | "popd" | "chdir" => cd = true,
            "rm" | "rmdir" => {
                rm = true;
                if dangerous_removal(&words[skip + 1..], cmd.expands, cwd, roots, home) {
                    return true;
                }
            }
            _ => {}
        }
    }
    cd && rm
}

fn path_like(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('~')
        || value.starts_with("./")
        || value.contains("//")
        || value.contains(":/")
        || value.split('/').any(|p| p == "..")
}

fn dangerous_removal(args: &[&str], expands: bool, cwd: &Path, roots: &[PathBuf], home: Option<&Path>) -> bool {
    let targets = args.iter().take_while(|a| **a != "--").filter(|a| !a.starts_with('-'));
    let after_dashdash = args.iter().skip_while(|a| **a != "--").skip(1);
    for target in targets.chain(after_dashdash) {
        if expands && target.contains('$') {
            return true;
        }
        let expanded = match (target.strip_prefix('~'), home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            (Some(_), _) => return true,
            _ => cwd.join(target),
        };
        let mut path = normalize(&expanded);
        // `dir/*` removes what `dir` holds: judge `dir`.
        while path.file_name().is_some_and(|n| n.to_str().is_some_and(|n| n.chars().all(|c| c == '*'))) {
            path = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        }
        let shown = path.to_string_lossy();
        if CRITICAL.contains(&shown.as_ref()) || home.is_some_and(|h| normalize(h) == path) {
            return true;
        }
        if roots.iter().chain(std::iter::once(&cwd.to_path_buf())).any(|r| normalize(r).starts_with(&path)) {
            return true;
        }
        let parts = path.components().filter(|c| matches!(c, Component::Normal(_))).count();
        if parts <= 1 || shown.contains(['*', '?', '[']) && path.parent().is_some_and(|p| p.as_os_str().len() <= 1) {
            return true;
        }
    }
    false
}

/// `.` and `..` resolved without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: serde_json::Value) -> SandboxConfig {
        serde_json::from_value(json).expect("config")
    }

    #[test]
    fn follows_claude_codes_order() {
        let off = SandboxConfig::default();
        assert!(!should_sandbox(&off, "ls", false));
        let on = cfg(serde_json::json!({"enabled": true, "excludedCommands": ["docker *", "npm run test:*"]}));
        assert!(should_sandbox(&on, "ls", false));
        assert!(!should_sandbox(&on, "ls", true), "dangerouslyDisableSandbox");
        assert!(!should_sandbox(&on, "docker ps", false));
        assert!(!should_sandbox(&on, "docker", false));
        assert!(!should_sandbox(&on, "npm run test -- --watch && docker ps 2>&1", false));
        assert!(should_sandbox(&on, "docker ps | grep x", false), "grep is not excluded");
        assert!(should_sandbox(&on, "docker ps > out.txt", false), "redirect to a file");
        assert!(should_sandbox(&on, "docker ps >/dev/null", false));
        assert!(should_sandbox(&on, "cd x && docker ps", false));
        assert!(should_sandbox(&on, "docker $(echo ps)", false));
        assert!(should_sandbox(&on, "docker <<EOF\nx\nEOF", false), "heredoc");
        let strict = cfg(serde_json::json!({"enabled": true, "allowUnsandboxedCommands": false,
            "excludedCommands": ["docker *"]}));
        assert!(should_sandbox(&strict, "ls", true), "the flag is ignored");
        assert!(should_sandbox(&strict, "docker ps", false), "exclusions need unsandboxed commands");
    }

    #[test]
    fn rule_syntax() {
        assert!(matches_rule("git status", "git status"));
        assert!(!matches_rule("git status", "git status -s"));
        assert!(matches_rule("npm test:*", "npm test -- --watch"));
        assert!(!matches_rule("npm test:*", "npm testing"));
        assert!(matches_rule("git * --dry-run", "git push --dry-run"));
    }

    #[test]
    fn exceptions_to_the_auto_allow() {
        let cwd = Path::new("/w/proj");
        let home = Some(Path::new("/Users/u"));
        let ask = |c: &str| needs_permission_anyway(c, cwd, &[], home);
        assert!(!ask("ls -la && cargo test"));
        assert!(!ask("RUST_LOG=debug cargo run"));
        assert!(ask("DYLD_INSERT_LIBRARIES=x.dylib ls"));
        assert!(ask("LANG=/tmp/x ls"));
        assert!(ask("echo hi > /dev/tcp/1.2.3.4/80"));
        assert!(ask("cd sub && rm file"));
        assert!(!ask("rm -rf target build/out"));
        assert!(ask("rm -rf /"));
        assert!(ask("rm -rf ~"));
        assert!(ask("rm -rf ~/"));
        assert!(ask("rm -rf /usr/local"));
        assert!(ask("rm -rf ."), "the working directory");
        assert!(ask("rm -rf .."), "its parent");
        assert!(ask("rm -rf $DIR/"));
        assert!(ask("rm -rf /w/*"));
        assert!(!ask("rmdir empty"));
    }
}
