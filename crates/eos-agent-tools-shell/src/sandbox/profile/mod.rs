//! The Seatbelt (SBPL) profile of one command: Claude Code's generated
//! profile (sandbox-runtime's `a9`), deny by default, read everywhere, write
//! only inside the write roots minus the protected paths.

mod base;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use self::base::{APPLE_EVENTS, BASE_HEAD, BASE_TAIL, PTY};

/// A path rule: a whole subtree, one file, or an anchored regex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rule {
    Subpath(PathBuf),
    Literal(PathBuf),
    Regex(String),
}

impl Rule {
    fn sbpl(&self) -> String {
        match self {
            Self::Subpath(p) => format!("(subpath {})", quote(&p.to_string_lossy())),
            Self::Literal(p) => format!("(literal {})", quote(&p.to_string_lossy())),
            Self::Regex(r) => format!("(regex {})", quote(r)),
        }
    }

    /// The directories above the rule's path (up to, not including, `/`).
    fn ancestors(&self) -> Vec<PathBuf> {
        let (base, skip) = match self {
            Self::Subpath(p) | Self::Literal(p) => (p.clone(), 1),
            // The directory the pattern starts in counts too.
            Self::Regex(r) => (regex_base(r), 0),
        };
        base.ancestors().skip(skip).filter(|a| a.parent().is_some()).map(Path::to_path_buf).collect()
    }
}

/// The literal directory a `^/a/b/...` regex starts in.
fn regex_base(regex: &str) -> PathBuf {
    let body = regex.trim_start_matches('^');
    let mut literal = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => literal.push(n),
                None => break,
            },
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '$' | '^' => break,
            c => literal.push(c),
        }
    }
    match literal.strip_suffix('/') {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let path = PathBuf::from(&literal);
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        }
    }
}

/// Everything one command's profile depends on. Paths must already be
/// resolved (Seatbelt matches real paths, `/private/tmp`, not `/tmp`).
#[derive(Debug, Default)]
pub(crate) struct ProfileSpec {
    /// Ends every denial's log line, so violations route back to the command.
    pub log_tag: String,
    /// The shared proxy's port; the only network destination allowed.
    pub proxy_port: Option<u16>,
    pub write_roots: Vec<PathBuf>,
    /// Single files writable outside the roots (the cwd side file).
    pub write_files: Vec<PathBuf>,
    pub deny_write: Vec<Rule>,
    pub deny_read: Vec<PathBuf>,
    pub allow_read: Vec<PathBuf>,
    /// Glob settings as anchored regexes (`allowWrite`, `denyRead`, `allowRead`).
    pub write_globs: Vec<String>,
    pub deny_read_globs: Vec<String>,
    pub allow_read_globs: Vec<String>,
    /// `credentials.files`: denied again after the `allowRead` rules, so only
    /// `own_allow_read` reopens anything below them.
    pub credential_deny: Vec<Rule>,
    /// The sandbox's own files readable inside denied paths (the TLS CA).
    pub own_allow_read: Vec<PathBuf>,
    pub allow_local_binding: bool,
    pub allow_unix_sockets: Vec<PathBuf>,
    pub allow_all_unix_sockets: bool,
    pub allow_mach_lookup: Vec<String>,
    pub allow_pty: bool,
    pub enable_weaker_network_isolation: bool,
    pub allow_apple_events: bool,
    /// Apple Events only to these bundle ids (a confined host command).
    pub apple_event_targets: Vec<String>,
    /// No `/usr/bin/security` and no Mach lookup of the Keychain's daemons,
    /// whatever the base allows (a confined host command).
    pub deny_keychain: bool,
    /// Reads outside `allow_read` are allowed but logged as the denials they
    /// would be (a confined host command's audit run).
    pub report_reads: bool,
}

/// The Mach services that serve the Keychain: `securityd` and `secd`.
const KEYCHAIN_SERVICES: &[&str] = &[
    "(global-name \"com.apple.SecurityServer\")",
    "(global-name-prefix \"com.apple.securityd\")",
    "(global-name-prefix \"com.apple.security.\")",
];

/// SBPL strings are JSON strings (Claude Code uses `JSON.stringify`).
pub(crate) fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Escapes regex metacharacters (Claude Code's `aZt`).
pub(crate) fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A path is usable in a profile when it has no NUL or line break.
pub(crate) fn usable(path: &Path) -> bool {
    !path.to_string_lossy().contains(['\0', '\n', '\r'])
}

impl ProfileSpec {
    pub(crate) fn render(&self) -> String {
        let tag = &self.log_tag;
        let mut p = String::with_capacity(8 * 1024);
        p += "(version 1)\n";
        let _ = writeln!(p, "(deny default (with message \"{tag}\"))\n\n; LogTag: {tag}\n");
        p += BASE_HEAD;
        if self.enable_weaker_network_isolation {
            p += "; trustd.agent - needed for Go TLS certificate verification (weaker network isolation)\n";
            p += "(allow mach-lookup (global-name \"com.apple.trustd.agent\"))\n";
        }
        if self.allow_apple_events {
            p += APPLE_EVENTS;
        }
        if !self.apple_event_targets.is_empty() {
            p += "; Apple Events to the named apps only\n";
            let targets: Vec<String> =
                self.apple_event_targets.iter().map(|t| format!("(appleevent-destination {})", quote(t))).collect();
            block(&mut p, "allow", &["appleevent-send"], &targets, tag);
            p += "(allow mach-lookup (global-name \"com.apple.coreservices.appleevents\"))\n";
        }
        if !self.allow_mach_lookup.is_empty() {
            p += "; User-specified XPC/Mach services\n";
            for name in &self.allow_mach_lookup {
                match name.strip_suffix('*') {
                    Some(prefix) => {
                        let _ = writeln!(p, "(allow mach-lookup (global-name-prefix {}))", quote(prefix));
                    }
                    None => {
                        let _ = writeln!(p, "(allow mach-lookup (global-name {}))", quote(name));
                    }
                }
            }
        }
        p += BASE_TAIL;
        self.network(&mut p);
        p += "\n; File read\n";
        self.file_read(&mut p);
        p += "\n; File write\n";
        self.file_write(&mut p);
        if self.allow_pty {
            p += PTY;
        }
        if self.deny_keychain {
            p += "\n; Keychain\n";
            block(&mut p, "deny", &["process-exec"], &["(literal \"/usr/bin/security\")".to_owned()], tag);
            let services: Vec<String> = KEYCHAIN_SERVICES.iter().map(|s| (*s).to_owned()).collect();
            block(&mut p, "deny", &["mach-lookup"], &services, tag);
        }
        p
    }

    fn network(&self, p: &mut String) {
        p.push_str("; Network\n");
        if self.allow_local_binding {
            p.push_str("(allow network-bind (local ip \"*:*\"))\n");
            p.push_str("(allow network-inbound (local ip \"*:*\"))\n");
            p.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        if self.allow_all_unix_sockets {
            p.push_str("(allow system-socket (socket-domain AF_UNIX))\n");
            p.push_str("(allow network-bind (local unix-socket (path-regex #\"^/\")))\n");
            p.push_str("(allow network-outbound (remote unix-socket (path-regex #\"^/\")))\n");
        } else if !self.allow_unix_sockets.is_empty() {
            p.push_str("(allow system-socket (socket-domain AF_UNIX))\n");
            for path in &self.allow_unix_sockets {
                let q = quote(&path.to_string_lossy());
                let _ = writeln!(p, "(allow network-bind (local unix-socket (subpath {q})))");
                let _ = writeln!(p, "(allow network-outbound (remote unix-socket (subpath {q})))");
            }
        }
        if let Some(port) = self.proxy_port {
            block(
                p,
                "deny",
                &["network-bind", "network-inbound"],
                &[format!("(local ip \"*:{port}\")")],
                &self.log_tag,
            );
            let _ = writeln!(p, "(allow network-outbound (remote ip \"localhost:{port}\"))");
        }
        p.push('\n');
    }

    fn file_read(&self, p: &mut String) {
        p.push_str("(allow file-read*)\n");
        if self.deny_read_rules().is_empty() {
            return;
        }
        let tag = &self.log_tag;
        let denies: Vec<String> = self.deny_read_rules().iter().map(Rule::sbpl).collect();
        if self.report_reads {
            reported(p, &["file-read*"], &denies, tag);
        } else {
            block(p, "deny", &["file-read*"], &denies, tag);
        }
        let own: Vec<String> = self.own_allow_read.iter().map(|a| Rule::Subpath(a.clone()).sbpl()).collect();
        let allows: Vec<String> = self
            .allow_read
            .iter()
            .map(|a| Rule::Subpath(a.clone()))
            .chain(self.allow_read_globs.iter().map(|g| Rule::Regex(g.clone())))
            .map(|r| r.sbpl())
            .chain(own.iter().cloned())
            .collect();
        block(p, "allow", &["file-read*"], &allows, tag);
        // Credential files stay unreadable whatever `allowRead` says.
        let credentials: Vec<String> = dedup(self.credential_deny.iter().map(Rule::sbpl));
        block(p, "deny", &["file-read*"], &credentials, tag);
        if !credentials.is_empty() {
            block(p, "allow", &["file-read*"], &own, tag);
        }
        if self.deny_read.iter().any(|d| d == Path::new("/")) {
            p.push_str("(allow file-read* (literal \"/\"))\n");
        }
        p.push_str("(allow file-read-metadata\n  (vnode-type DIRECTORY))\n");
        // A denied path must not be moved away and back to escape its rule.
        block(p, "deny", &["file-write-unlink", "file-write-create"], &with_ancestors(&self.deny_read_rules()), tag);
        block(p, "allow", &["file-write-unlink", "file-write-create"], &self.write_root_rules(), tag);
    }

    fn deny_read_rules(&self) -> Vec<Rule> {
        let paths = self.deny_read.iter().map(|d| Rule::Subpath(d.clone()));
        let globs = self.deny_read_globs.iter().map(|g| Rule::Regex(g.clone()));
        paths.chain(globs).chain(self.credential_deny.iter().cloned()).collect()
    }

    fn write_root_rules(&self) -> Vec<String> {
        let paths = self.write_roots.iter().map(|r| Rule::Subpath(r.clone()));
        paths.chain(self.write_globs.iter().map(|g| Rule::Regex(g.clone()))).map(|r| r.sbpl()).collect()
    }

    fn file_write(&self, p: &mut String) {
        let tag = &self.log_tag;
        let roots = self.write_root_rules();
        block(p, "allow", &["file-write*"], &roots, tag);
        let denies: Vec<String> = dedup(self.deny_write.iter().map(Rule::sbpl));
        block(p, "deny", &["file-write*"], &denies, tag);
        block(p, "deny", &["file-write-unlink", "file-write-create"], &with_ancestors(&self.deny_write), tag);
        let files: Vec<String> = self.write_files.iter().map(|f| Rule::Literal(f.clone()).sbpl()).collect();
        block(p, "allow", &["file-write*"], &files, tag);
    }
}

/// Each rule plus a `literal` for every directory above it (Claude Code's `x1`).
fn with_ancestors(rules: &[Rule]) -> Vec<String> {
    let mut out = Vec::new();
    for rule in rules {
        out.push(rule.sbpl());
        out.extend(rule.ancestors().into_iter().map(|a| Rule::Literal(a).sbpl()));
    }
    dedup(out.into_iter())
}

fn dedup(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|i| seen.insert(i.clone())).collect()
}

/// `(<verb> <ops>\n  <filter>…\n  (with message "<tag>"))`, nothing when empty
/// (Claude Code's `ao`).
fn block(p: &mut String, verb: &str, ops: &[&str], filters: &[String], tag: &str) {
    rule(p, verb, ops, filters, "", tag);
}

/// As [`block`], but allowed and logged as the denial it would be.
fn reported(p: &mut String, ops: &[&str], filters: &[String], tag: &str) {
    rule(p, "allow", ops, filters, "(with report) ", tag);
}

fn rule(p: &mut String, verb: &str, ops: &[&str], filters: &[String], modifier: &str, tag: &str) {
    if filters.is_empty() {
        return;
    }
    let _ = writeln!(p, "({verb} {}", ops.join(" "));
    for f in filters {
        let _ = writeln!(p, "  {f}");
    }
    let _ = writeln!(p, "  {modifier}(with message \"{tag}\"))");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProfileSpec {
        ProfileSpec {
            log_tag: "EOS_c1_p1_SBX".into(),
            proxy_port: Some(7000),
            write_roots: vec!["/w/proj".into(), "/s/tmp".into()],
            write_files: vec!["/s/tasks/b1.cwd".into()],
            deny_write: vec![
                Rule::Literal("/w/proj/.zshrc".into()),
                Rule::Regex(format!("^{}/(.*/)?\\.git/hooks(/.*)?$", regex_escape("/w/proj"))),
            ],
            ..ProfileSpec::default()
        }
    }

    #[test]
    fn base_matches_claude_codes_profile() {
        let text = spec().render();
        assert!(text.starts_with("(version 1)\n(deny default (with message \"EOS_c1_p1_SBX\"))\n\n; LogTag: EOS_c1_p1_SBX\n\n; Essential permissions - based on Chrome sandbox policy\n"));
        // The captured profile of Claude Code (research): same sysctl list, same mach services.
        assert_eq!(text.matches("(sysctl-name").count(), 59);
        assert_eq!(text.matches("(global-name \"").count(), 15);
        assert!(!text.contains("(allow network*)"));
        assert!(text.contains(
            "(deny network-bind network-inbound\n  (local ip \"*:7000\")\n  (with message \"EOS_c1_p1_SBX\"))\n\
             (allow network-outbound (remote ip \"localhost:7000\"))\n"
        ));
    }

    #[test]
    fn write_rules_allow_roots_and_protect_paths() {
        let text = spec().render();
        let write = &text[text.find("; File write").expect("section")..];
        assert_eq!(
            write,
            "; File write\n(allow file-write*\n  (subpath \"/w/proj\")\n  (subpath \"/s/tmp\")\n  (with message \"EOS_c1_p1_SBX\"))\n\
             (deny file-write*\n  (literal \"/w/proj/.zshrc\")\n  (regex \"^/w/proj/(.*/)?\\\\.git/hooks(/.*)?$\")\n  (with message \"EOS_c1_p1_SBX\"))\n\
             (deny file-write-unlink file-write-create\n  (literal \"/w/proj/.zshrc\")\n  (literal \"/w/proj\")\n  (literal \"/w\")\n  \
             (regex \"^/w/proj/(.*/)?\\\\.git/hooks(/.*)?$\")\n  (with message \"EOS_c1_p1_SBX\"))\n\
             (allow file-write*\n  (literal \"/s/tasks/b1.cwd\")\n  (with message \"EOS_c1_p1_SBX\"))\n"
        );
    }

    #[test]
    fn glob_settings_become_regexes() {
        let mut s = spec();
        s.write_globs = vec![r"^/w/out/[^/]*\.log(/.*)?$".into()];
        s.deny_read_globs = vec![r"^/w/(.*/)?[^/]*\.pem(/.*)?$".into()];
        s.allow_read_globs = vec![r"^/w/pub/[^/]*\.pem(/.*)?$".into()];
        let text = s.render();
        assert!(
            text.contains("(allow file-write*\n  (subpath \"/w/proj\")\n  (subpath \"/s/tmp\")\n  (regex \"^/w/out/")
        );
        assert!(text.contains("(deny file-read*\n  (regex \"^/w/(.*/)?"), "{text}");
        assert!(text.contains("(allow file-read*\n  (regex \"^/w/pub/"), "{text}");
    }

    #[test]
    fn read_denials_and_options() {
        let mut s = spec();
        s.deny_read = vec!["/Users/u/.ssh".into()];
        s.allow_read = vec!["/Users/u/.ssh/known_hosts".into()];
        s.allow_local_binding = true;
        s.allow_mach_lookup = vec!["com.apple.CoreSimulator.*".into(), "com.x".into()];
        s.allow_unix_sockets = vec!["/var/run/docker.sock".into()];
        let text = s.render();
        assert!(text.contains("(deny file-read*\n  (subpath \"/Users/u/.ssh\")\n"));
        assert!(text.contains("(allow file-read*\n  (subpath \"/Users/u/.ssh/known_hosts\")\n"));
        assert!(text.contains("(allow network-bind (local ip \"*:*\"))\n"));
        assert!(text.contains("(allow mach-lookup (global-name-prefix \"com.apple.CoreSimulator.\"))\n"));
        assert!(text.contains("(allow network-outbound (remote unix-socket (subpath \"/var/run/docker.sock\")))\n"));
    }

    #[test]
    fn credential_files_stay_denied_after_allow_read() {
        let mut s = spec();
        s.deny_read = vec!["/Users/u/.aws".into()];
        s.allow_read = vec!["/Users/u".into()];
        s.credential_deny = vec![Rule::Subpath("/Users/u/.netrc".into())];
        s.own_allow_read = vec!["/t/srt-ca-1/ca.crt".into()];
        let text = s.render();
        let read = &text[text.find("; File read").expect("read")..text.find("; File write").expect("write")];
        let deny = "(deny file-read*\n  (subpath \"/Users/u/.aws\")\n  (subpath \"/Users/u/.netrc\")\n";
        let again = "(deny file-read*\n  (subpath \"/Users/u/.netrc\")\n  (with message \"EOF_TAG\"))\n\
                     (allow file-read*\n  (subpath \"/t/srt-ca-1/ca.crt\")\n";
        let again = again.replace("EOF_TAG", "EOS_c1_p1_SBX");
        assert!(read.contains(deny), "{read}");
        let at = read.find(&again).expect("denied again");
        assert!(at > read.find("(subpath \"/Users/u\")").expect("allowRead"), "after allowRead: {read}");
        assert!(read.contains("(deny file-write-unlink file-write-create\n  (subpath \"/Users/u/.aws\")"), "{read}");
    }

    #[test]
    fn strings_are_json_quoted() {
        assert_eq!(quote(r#"/a "b"\c"#), r#""/a \"b\"\\c""#);
        assert_eq!(regex_escape("/a.b(c)"), r"/a\.b\(c\)");
        assert!(!usable(Path::new("/a\nb")) && !usable(Path::new("/a\0b")) && usable(Path::new("/a b")));
        assert_eq!(regex_base(r"^/w/pr\.oj/(.*/)?\.git$"), PathBuf::from("/w/pr.oj"));
    }
}
