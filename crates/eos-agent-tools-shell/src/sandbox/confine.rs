//! Confinement of one host command, not an agent's shell call: the host
//! names the program, its whole environment, what it may read and write,
//! the hosts it may reach and the apps it may send Apple Events to, and the
//! OS sandbox denies everything else. It runs on the profile builder, the
//! proxy and the violation monitor the agents' commands share, with none of
//! Claude Code's settings.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::paths::WRITABLE_DEVICES;
use super::profile::{ProfileSpec, usable};
use super::proxy::filter::HostPolicy;
use super::proxy::{Lease, Owner};
use super::seatbelt::resolved;
use super::violations::{Watch, verdict};

/// Readable by every confined command: the system's programs, libraries,
/// devices, settings and time zones, never a user's files.
const SYSTEM_READ: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library",
    "/opt/homebrew",
    "/dev",
    "/private/etc",
    "/private/var/db/timezone",
    "/private/var/select",
];

/// The root's links into `/private`, readable as links only (Seatbelt
/// matches what they resolve to), so paths through them resolve.
const ROOT_LINKS: &[&str] = &["/etc", "/var", "/tmp"];

/// One host command and everything it may touch. Every path is absolute.
#[derive(Debug, Clone, Default)]
pub struct ConfineSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Where the host starts it; readable.
    pub cwd: PathBuf,
    /// The whole environment it sees: nothing of the host's is inherited.
    pub env: Vec<(String, String)>,
    /// Readable besides the system's folders, the program, `cwd`, the
    /// write paths and `tmpdir`.
    pub read_paths: Vec<PathBuf>,
    /// Readable and writable.
    pub write_paths: Vec<PathBuf>,
    /// Host names the proxy lets it reach (`*.example.com` for subdomains);
    /// empty: no network. Loopback and private networks are never reached.
    /// Ignored in an audit.
    pub hosts: Vec<String>,
    /// Bundle ids it may send Apple Events to. Ignored in an audit.
    pub apple_events: Vec<String>,
    /// Its `TMPDIR`: made if missing, readable and writable.
    pub tmpdir: PathBuf,
    /// Reads outside its paths are allowed and reported as violations, so
    /// one run shows all of them. Since it may then read every file the
    /// user can, nothing may leave: every connection and Apple Event is
    /// denied and reported, whatever `hosts` and `apple_events` name.
    /// Writes and the Keychain stay denied.
    pub audit: bool,
}

/// The confined command, for the host to start with its own process runner.
#[derive(Debug)]
pub struct Confined {
    /// `/usr/bin/sandbox-exec`, which execs the program in place (same pid).
    pub program: PathBuf,
    /// The profile, then the spec's program and arguments.
    pub args: Vec<String>,
    /// The child's whole environment: the spec's, with `TMPDIR` and the
    /// proxy's variables over it. Start the child with exactly this.
    pub env: Vec<(String, String)>,
    /// Held while the child runs; finished once it has exited.
    pub guard: ConfineGuard,
}

/// Keeps the command's proxy credentials valid and collects its violations.
#[derive(Debug)]
pub struct ConfineGuard {
    pub(super) watch: Watch,
    pub(super) lease: Option<Lease>,
    /// Whether the shared `log stream` reads the OS sandbox's lines.
    pub(super) watched: bool,
}

impl ConfineGuard {
    /// What the command was denied or, in an audit, read outside its paths.
    /// Its proxy credentials end here.
    pub async fn finish(self) -> ConfineReport {
        let Self { watch, lease, watched } = self;
        drop(lease);
        let (violations, more) = watch.finish_confined().await;
        ConfineReport { violations, more, watched }
    }
}

/// What a confined command was denied or, in an audit, read outside its
/// paths. It was denied nothing only when `violations` is empty, `more` is
/// 0 and `watched` is true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfineReport {
    /// Each once, at most 50, in the order they came.
    pub violations: Vec<Violation>,
    /// How many more there were.
    pub more: usize,
    /// Whether the OS sandbox's own lines were read. When `log stream`
    /// could not start, `violations` holds only the proxy's refusals.
    pub watched: bool,
}

/// One thing a confined command was denied (or, in an audit, read).
/// Other refusals (sysctl, IOKit, signals …) are enforced but not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    FileRead {
        path: PathBuf,
    },
    FileWrite {
        path: PathBuf,
    },
    /// The host and port the proxy refused (`example.com:443`), or the
    /// socket as the OS sandbox names it (`remote:*:443`; `local:*:8080`
    /// for a listen), which carries no address.
    Network {
        target: String,
    },
    Exec {
        path: PathBuf,
    },
    MachLookup {
        service: String,
    },
    AppleEvent {
        target: String,
    },
}

impl Violation {
    /// From the kernel's `cat(9) deny(1) file-read-data /x` (`allow` in an
    /// audit) or the proxy's `deny network-outbound host:443 (why)`.
    pub(super) fn parse(line: &str) -> Option<Self> {
        let (operation, target) = verdict(line)?.1.split_once(' ')?;
        let path = || PathBuf::from(target);
        Some(match operation {
            op if op.starts_with("file-read") => Self::FileRead { path: path() },
            op if op.starts_with("file-write") => Self::FileWrite { path: path() },
            op if op.starts_with("process-exec") => Self::Exec { path: path() },
            op if op.starts_with("network") => {
                Self::Network { target: target.split_once(" (").map_or(target, |(t, _)| t).to_owned() }
            }
            "mach-lookup" => Self::MachLookup { service: target.to_owned() },
            "appleevent-send" => Self::AppleEvent { target: target.to_owned() },
            _ => return None,
        })
    }
}

/// Why a command could not be confined.
#[derive(Debug)]
pub enum ConfineError {
    /// The OS sandbox cannot run on this machine; nothing runs unconfined.
    Unavailable,
    /// A path the profile cannot name, or the temp folder could not be made.
    Io(io::Error),
}

impl fmt::Display for ConfineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("the sandbox cannot start on this machine"),
            Self::Io(e) => write!(f, "the command cannot be confined: {e}"),
        }
    }
}

impl std::error::Error for ConfineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable => None,
            Self::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for ConfineError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The Seatbelt profile of `spec`: deny by default, read only the system's
/// folders and the spec's paths, write only its write paths, no Keychain.
pub(super) fn profile(spec: &ConfineSpec, log_tag: &str, proxy_port: Option<u16>) -> io::Result<String> {
    let named =
        [&[spec.program.clone(), spec.cwd.clone(), spec.tmpdir.clone()][..], &spec.read_paths, &spec.write_paths]
            .concat();
    if let Some(bad) = named.iter().find(|p| !p.is_absolute() || !usable(p)) {
        return Err(invalid(format!(
            "not an absolute path a sandbox profile can hold: {:?}",
            bad.display().to_string()
        )));
    }
    if spec.program.to_str().is_none() {
        return Err(invalid("the program's path is not UTF-8".to_owned()));
    }
    let writable = [&spec.write_paths[..], std::slice::from_ref(&spec.tmpdir)].concat();
    let mut allow_read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    allow_read.extend(resolved(&named));
    allow_read.extend(ROOT_LINKS.iter().map(PathBuf::from));
    let mut write_roots = resolved(&writable);
    write_roots.extend(WRITABLE_DEVICES.iter().map(PathBuf::from));
    let profile = ProfileSpec {
        log_tag: log_tag.to_owned(),
        proxy_port,
        write_roots,
        deny_read: vec![PathBuf::from("/")],
        allow_read,
        apple_event_targets: if spec.audit { Vec::new() } else { spec.apple_events.clone() },
        deny_keychain: true,
        report_reads: spec.audit,
        ..ProfileSpec::default()
    };
    Ok(profile.render())
}

/// The proxy's owner for a confined command: only its hosts (none in an
/// audit), never asked about others, public addresses only.
pub(super) fn owner(cmd_id: &str, spec: &ConfineSpec) -> Owner {
    let allowed = if spec.audit { Vec::new() } else { spec.hosts.clone() };
    let hosts = HostPolicy { allowed, strict: true, public_only: true, ..HostPolicy::default() };
    Owner {
        agent_id: String::new(),
        session: cmd_id.to_owned(),
        cmd_id: cmd_id.to_owned(),
        hosts: Arc::new(hosts),
        tls: None,
        vault: Arc::default(),
        plaintext_inject: false,
        sigv4: Default::default(),
    }
}

/// The spec's environment with the sandbox's variables over it.
pub(super) fn environment(mut env: Vec<(String, String)>, sandbox: Vec<(String, String)>) -> Vec<(String, String)> {
    env.retain(|(name, _)| !sandbox.iter().any(|(own, _)| own == name));
    env.extend(sandbox);
    env
}

fn invalid(text: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ConfineSpec {
        ConfineSpec {
            program: "/opt/homebrew/bin/node".into(),
            args: vec!["main.js".into()],
            cwd: "/pkg".into(),
            read_paths: vec!["/pkg".into()],
            write_paths: vec!["/pkg/data".into()],
            hosts: vec!["api.example.com".into()],
            apple_events: vec!["com.apple.MobileSMS".into()],
            tmpdir: "/calls/c1".into(),
            ..ConfineSpec::default()
        }
    }

    fn section<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
        let start = text.find(from).expect("start");
        let end = text[start..].find(to).map_or(text.len(), |e| start + e);
        &text[start..end]
    }

    #[test]
    fn reads_only_the_system_and_its_own_paths() {
        let text = profile(&spec(), "T_SBX", Some(7000)).expect("profile");
        let read = section(&text, "; File read", "; File write");
        assert!(read.contains("(deny file-read*\n  (subpath \"/\")\n  (with message \"T_SBX\"))\n"), "{read}");
        let allowed = section(read, "(allow file-read*\n", "(with message");
        for path in ["/usr", "/System", "/Library", "/opt/homebrew", "/dev", "/private/etc", "/pkg", "/pkg/data"] {
            assert!(allowed.contains(&format!("(subpath \"{path}\")")), "{path}: {allowed}");
        }
        assert!(allowed.contains("(subpath \"/opt/homebrew/bin/node\")"), "the program: {allowed}");
        assert!(allowed.contains("(subpath \"/calls/c1\")"), "the temp folder: {allowed}");
        assert!(allowed.contains("(subpath \"/var\")") && !allowed.contains("(subpath \"/private/var\")"), "{allowed}");
        assert!(!allowed.contains("/Users"), "{allowed}");
    }

    #[test]
    fn writes_only_its_write_paths_and_the_devices() {
        let text = profile(&spec(), "T_SBX", None).expect("profile");
        let write = section(&text, "; File write", "; Keychain");
        assert_eq!(
            section(write, "(allow file-write*", "(with message"),
            "(allow file-write*\n  (subpath \"/pkg/data\")\n  (subpath \"/calls/c1\")\n  (subpath \"/dev/stdout\")\n  \
             (subpath \"/dev/stderr\")\n  (subpath \"/dev/null\")\n  (subpath \"/dev/tty\")\n  \
             (subpath \"/dev/dtracehelper\")\n  (subpath \"/dev/autofs_nowait\")\n  "
        );
        assert!(!text.contains("network-bind (local ip \"*:*\")"), "no listening sockets");
        assert!(!text.contains("(allow network-outbound"), "no proxy, no network: {text}");
    }

    #[test]
    fn the_keychain_apple_events_and_the_proxy() {
        let text = profile(&spec(), "T_SBX", Some(7000)).expect("profile");
        assert!(text.ends_with(
            "; Keychain\n(deny process-exec\n  (literal \"/usr/bin/security\")\n  (with message \"T_SBX\"))\n\
             (deny mach-lookup\n  (global-name \"com.apple.SecurityServer\")\n  \
             (global-name-prefix \"com.apple.securityd\")\n  (global-name-prefix \"com.apple.security.\")\n  \
             (with message \"T_SBX\"))\n"
        ));
        assert!(text.contains(
            "(allow appleevent-send\n  (appleevent-destination \"com.apple.MobileSMS\")\n  (with message \"T_SBX\"))\n\
             (allow mach-lookup (global-name \"com.apple.coreservices.appleevents\"))\n"
        ));
        assert!(!text.contains("(allow lsopen)"), "opening apps and URLs stays denied");
        assert!(text.contains("(allow network-outbound (remote ip \"localhost:7000\"))\n"));
    }

    #[test]
    fn an_audit_reports_reads_and_still_denies_writes() {
        let text = profile(&ConfineSpec { audit: true, ..spec() }, "T_SBX", None).expect("profile");
        assert!(text.contains("(allow file-read*\n  (subpath \"/\")\n  (with report) (with message \"T_SBX\"))\n"));
        assert!(!text.contains("(deny file-read*"), "{text}");
        assert!(text.contains("(deny file-write-unlink file-write-create\n  (subpath \"/\")\n"), "{text}");
        assert!(text.starts_with("(version 1)\n(deny default (with message \"T_SBX\"))"));
        assert!(!text.contains("appleevent"), "it may read anything, so it sends no Apple Event: {text}");
    }

    #[test]
    fn paths_must_be_absolute() {
        let relative = ConfineSpec { read_paths: vec!["data".into()], ..spec() };
        assert_eq!(profile(&relative, "T", None).expect_err("relative").kind(), io::ErrorKind::InvalidInput);
        let broken = ConfineSpec { write_paths: vec!["/a\nb".into()], ..spec() };
        assert_eq!(profile(&broken, "T", None).expect_err("line break").kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn violations_from_the_kernel_and_the_proxy() {
        let parse = Violation::parse;
        assert_eq!(
            parse("node(9) deny(1) file-read-data /Users/u/.ssh/id rsa"),
            Some(Violation::FileRead { path: "/Users/u/.ssh/id rsa".into() })
        );
        assert_eq!(parse("cat(9) allow file-read-metadata /x"), Some(Violation::FileRead { path: "/x".into() }));
        assert_eq!(parse("sh(9) deny(1) file-write-create /x"), Some(Violation::FileWrite { path: "/x".into() }));
        assert_eq!(
            parse("sh(9) deny(1) process-exec* /usr/bin/security"),
            Some(Violation::Exec { path: "/usr/bin/security".into() })
        );
        assert_eq!(
            parse("deny network-outbound evil.com:443 (host is not on the allow list)"),
            Some(Violation::Network { target: "evil.com:443".into() })
        );
        assert_eq!(
            parse("nc(9) deny(1) network-bind local:*:8080"),
            Some(Violation::Network { target: "local:*:8080".into() })
        );
        assert_eq!(
            parse("Google Chrome(9) deny(1) mach-lookup com.apple.SecurityServer"),
            Some(Violation::MachLookup { service: "com.apple.SecurityServer".into() })
        );
        assert_eq!(
            parse("osascript(9) deny(1) appleevent-send com.apple.mail"),
            Some(Violation::AppleEvent { target: "com.apple.mail".into() })
        );
        assert_eq!(parse("sh(9) deny(1) sysctl-read kern.iossupportversion"), None);
        assert_eq!(parse("… and 3 more"), None);
    }

    #[test]
    fn the_sandbox_variables_win() {
        let env = environment(
            vec![("TMPDIR".into(), "/mine".into()), ("TOKEN".into(), "t".into())],
            vec![("TMPDIR".into(), "/calls/c1".into())],
        );
        assert_eq!(env, [("TOKEN".to_owned(), "t".to_owned()), ("TMPDIR".to_owned(), "/calls/c1".to_owned())]);
    }

    #[test]
    fn owners_judge_only_their_hosts() {
        use super::super::proxy::filter::Verdict;
        let listed = owner("c1", &spec());
        assert_eq!(listed.hosts.judge("api.example.com", 443), Verdict::Allow);
        assert_eq!(listed.hosts.judge("other.com", 443), Verdict::Deny("host is not on the allow list"));
        let none = owner("c2", &ConfineSpec { hosts: Vec::new(), ..spec() });
        assert_eq!(none.hosts.judge("api.example.com", 443), Verdict::Deny("host is not on the allow list"));
        let audit = owner("c3", &ConfineSpec { audit: true, ..spec() });
        assert_eq!(audit.hosts.judge("api.example.com", 443), Verdict::Deny("host is not on the allow list"));
    }

    #[test]
    fn spec_paths_resolve_like_seatbelt() {
        let dir = std::env::temp_dir().join(format!("confine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let text = profile(&ConfineSpec { tmpdir: dir.join("t"), ..spec() }, "T", None).expect("profile");
        let real = std::fs::canonicalize(&dir).expect("real").join("t");
        assert!(text.contains(&format!("(subpath \"{}\")", real.display())), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
