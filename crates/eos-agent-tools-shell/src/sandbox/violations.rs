//! Sandbox violations: one shared `log stream` child reads the kernel's
//! "Sandbox: … deny" lines (and, for a command that asks, the "allow" lines
//! of rules that report) and routes them to the command whose tag ends the
//! line; the proxy adds the connections it refused. A command's lines come
//! back as Claude Code's `<sandbox_violations>` block.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, OnceCell};

/// Lines kept per command; the rest are counted.
const MAX_LINES: usize = 50;
/// How long after a failed command exits we wait for its lines (they were
/// measured to arrive before the exit; this covers a busy `logd`). The whole
/// window: the lines that came first are often the system's own denials, and
/// the one that made the command fail may still be on its way.
const GRACE: Duration = Duration::from_millis(50);
const READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Kernel lines that are noise, not something the command was refused:
/// exactly sandbox-runtime's list (`_1`), matched as it matches them
/// (anywhere in the line). macOS 27's own denials in nearly every process
/// (`sysctl-read kern.iossupportversion`, `system-info vfs.disk-space`) are
/// reported as Claude Code reports them: the same reads are the real cause
/// when a command asks for those values (`sysctl kern.iossupportversion`
/// fails on exactly that line), and no rule tells the two apart.
const NOISE: &[&str] = &["mDNSResponder", "mach-lookup com.apple.diagnosticd", "mach-lookup com.apple.analyticsd"];

#[derive(Debug, Default)]
struct Collector {
    command: String,
    /// The agent's `ignoreViolations`.
    ignore: Arc<BTreeMap<String, Vec<String>>>,
    /// Takes the kernel's reports of what it allowed but would have denied.
    reports: bool,
    lines: Vec<String>,
    more: usize,
}

/// Every command's violations, by command id.
#[derive(Debug)]
pub(crate) struct Violations {
    /// Random per process, so another process's tags never match.
    proc_tag: String,
    by_cmd: Mutex<HashMap<String, Collector>>,
    monitor: OnceCell<bool>,
}

impl Violations {
    pub(crate) fn new() -> Arc<Self> {
        let proc_tag: String = std::iter::repeat_with(fastrand::alphanumeric).take(10).collect();
        Arc::new(Self { proc_tag, by_cmd: Mutex::default(), monitor: OnceCell::new() })
    }

    /// The message that ends the kernel's log line for `cmd_id`'s denials.
    pub(crate) fn tag(&self, cmd_id: &str) -> String {
        format!("EOS_{cmd_id}_{}_SBX", self.proc_tag)
    }

    fn cmd_of<'a>(&self, tag: &'a str) -> Option<&'a str> {
        let rest = tag.trim().strip_prefix("EOS_")?;
        rest.strip_suffix("_SBX")?.strip_suffix(self.proc_tag.as_str())?.strip_suffix('_')
    }

    /// Starts the shared `log stream` once and waits until it reads. False
    /// when it cannot run: commands still run, their reports are off.
    pub(crate) async fn ensure_monitor(self: &Arc<Self>) -> bool {
        *self
            .monitor
            .get_or_init(|| async {
                let ready = Arc::new(Notify::new());
                tokio::spawn(monitor(Arc::downgrade(self), Arc::clone(&ready), format!("_{}_SBX", self.proc_tag)));
                let ok = tokio::time::timeout(READY_TIMEOUT, ready.notified()).await.is_ok();
                if !ok {
                    tracing::debug!("sandbox violation reports are off: `log stream` did not start");
                }
                ok
            })
            .await
    }

    /// Collects `cmd_id`'s violations, minus those its agent's `ignore`
    /// names, until the returned watch is finished; with `reports`, also
    /// what its profile allowed but asked to hear about.
    pub(crate) fn watch(
        self: &Arc<Self>,
        cmd_id: &str,
        command: &str,
        ignore: Arc<BTreeMap<String, Vec<String>>>,
        reports: bool,
    ) -> Watch {
        let collector = Collector { command: command.to_owned(), ignore, reports, ..Collector::default() };
        lock(&self.by_cmd).insert(cmd_id.to_owned(), collector);
        Watch { violations: Arc::clone(self), cmd_id: cmd_id.to_owned() }
    }

    /// Adds a line for a command that is being watched.
    pub(crate) fn record(&self, cmd_id: &str, line: &str) {
        self.add(cmd_id, line, false);
    }

    fn add(&self, cmd_id: &str, line: &str, report: bool) {
        let mut map = lock(&self.by_cmd);
        let Some(c) = map.get_mut(cmd_id) else { return };
        if (report && !c.reports) || ignored(&c.ignore, line, &c.command) || c.lines.iter().any(|l| l == line) {
            return;
        }
        if c.lines.len() < MAX_LINES {
            c.lines.push(line.to_owned());
        } else {
            c.more += 1;
        }
    }

    /// One ndjson line of `log stream`.
    fn route(&self, json: &str) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else { return };
        let Some(message) = event.get("eventMessage").and_then(|m| m.as_str()) else { return };
        let Some((text, tag)) = message.rsplit_once('\n') else { return };
        let Some(cmd) = self.cmd_of(tag) else { return };
        if let Some(line) = parse_line(text) {
            self.record(cmd, line);
        } else if let Some(line) = parse_report(text) {
            self.add(cmd, line, true);
        }
    }
}

/// `bash(123) deny(1) file-write-create /x` from `Sandbox: bash(123) deny(1) …`,
/// unless it is noise.
fn parse_line(text: &str) -> Option<&str> {
    let line = text.trim().strip_prefix("Sandbox:")?.trim();
    (line.contains("deny") && !NOISE.iter().any(|n| line.contains(n))).then_some(line)
}

/// `cat(123) allow file-read-data /x` from a rule that allows and reports
/// (`with report`), unless it is noise.
fn parse_report(text: &str) -> Option<&str> {
    let line = text.trim().strip_prefix("Sandbox:")?.trim();
    (line.split_whitespace().nth(1) == Some("allow") && !NOISE.iter().any(|n| line.contains(n))).then_some(line)
}

/// Claude Code's `ignoreViolations`: `"*"` paths are ignored for every
/// command, other keys for commands containing the key.
fn ignored(ignore: &BTreeMap<String, Vec<String>>, line: &str, command: &str) -> bool {
    ignore.iter().any(|(key, paths)| {
        (key == "*" || command.contains(key.as_str())) && paths.iter().any(|p| line.contains(p.as_str()))
    })
}

async fn monitor(violations: Weak<Violations>, ready: Arc<Notify>, suffix: String) {
    let predicate = format!("eventMessage ENDSWITH \"{suffix}\"");
    let mut backoff = Duration::from_secs(1);
    loop {
        let child = Command::new("/usr/bin/log")
            .args(["stream", "--style", "ndjson", "--predicate", &predicate])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = child else {
            tracing::debug!("sandbox violation reports are off: `log stream` could not start");
            return;
        };
        let Some(stdout) = child.stdout.take() else { return };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(v) = violations.upgrade() else { return };
            if line.starts_with('{') {
                v.route(&line);
            } else {
                // "Filtering the log data using …": it reads from here on.
                ready.notify_one();
            }
        }
        let _ = child.wait().await;
        if violations.strong_count() == 0 {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One command's collector; dropping it forgets the command.
#[derive(Debug)]
pub(crate) struct Watch {
    violations: Arc<Violations>,
    cmd_id: String,
}

impl Watch {
    pub(crate) fn cmd_id(&self) -> &str {
        &self.cmd_id
    }

    /// The command's lines. After a failure the late ones get a moment to
    /// arrive; a success takes what has arrived (as Claude Code does).
    pub(crate) async fn finish(self, failed: bool) -> Vec<String> {
        if failed {
            tokio::time::sleep(GRACE).await;
        }
        tokio::task::yield_now().await; // lines already read but not yet routed
        let c = lock(&self.violations.by_cmd).remove(&self.cmd_id).unwrap_or_default();
        let mut lines = c.lines;
        if c.more > 0 {
            lines.push(format!("… and {} more", c.more));
        }
        lines
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        lock(&self.violations.by_cmd).remove(&self.cmd_id);
    }
}

/// `text` with Claude Code's block appended (`annotateStderrWithSandboxFailures`).
pub(crate) fn annotate(text: &str, lines: &[String]) -> String {
    if lines.is_empty() {
        return text.to_owned();
    }
    let mut out = format!("{text}\n<sandbox_violations>\n");
    for line in lines {
        out += line;
        out.push('\n');
    }
    out += "</sandbox_violations>";
    out
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_kernel_lines_by_tag() {
        let ignore = BTreeMap::from([("*".to_owned(), vec!["/ignored".to_owned()])]);
        let v = Violations::new();
        let watch = v.watch("c1", "touch /x", Arc::new(ignore), false);
        let other = v.watch("c2", "ls", Arc::default(), false);
        let tag = v.tag("c1");
        let event = |msg: String| serde_json::json!({"eventMessage": msg}).to_string();
        v.route(&event(format!("Sandbox: touch(9) deny(1) file-write-create /x\n{tag}")));
        v.route(&event(format!("Sandbox: touch(9) deny(1) file-write-create /x\n{tag}")));
        v.route(&event(format!("Sandbox: touch(9) deny(1) file-write-create /ignored/y\n{tag}")));
        v.route(&event(format!("Sandbox: mDNSResponder(1) deny(1) network\n{tag}")));
        v.route(&event("Sandbox: touch(9) deny(1) file-write-create /z\nEOS_c1_otherproc_SBX".into()));
        v.record("c1", "deny network-outbound evil.com:443 (host is not on the allow list)");
        v.route(&event(format!("Sandbox: cat(9) allow file-read-data /etc/passwd\n{tag}")));
        assert_eq!(
            watch.finish(true).await,
            [
                "touch(9) deny(1) file-write-create /x",
                "deny network-outbound evil.com:443 (host is not on the allow list)"
            ]
        );
        assert!(other.finish(false).await.is_empty());
    }

    #[tokio::test]
    async fn reports_reach_only_the_commands_that_ask_for_them() {
        let v = Violations::new();
        let audit = v.watch("c1", "cat", Arc::default(), true);
        let event = |msg: String| serde_json::json!({"eventMessage": msg}).to_string();
        v.route(&event(format!("Sandbox: cat(9) allow file-read-data /Users/u/.ssh/id\n{}", v.tag("c1"))));
        v.route(&event(format!("Sandbox: cat(9) deny(1) file-write-create /x\n{}", v.tag("c1"))));
        v.route(&event(format!("Sandbox: cat(9) allow mach-lookup com.apple.diagnosticd\n{}", v.tag("c1"))));
        v.route(&event(format!("1 duplicate report for Sandbox: cat(9) allow file-read-data /y\n{}", v.tag("c1"))));
        assert_eq!(
            audit.finish(false).await,
            ["cat(9) allow file-read-data /Users/u/.ssh/id", "cat(9) deny(1) file-write-create /x"]
        );
    }

    #[test]
    fn drops_only_sandbox_runtimes_noise() {
        for noise in [
            "Sandbox: mDNSResponder(88) deny(1) network-outbound /private/var/run/mDNSResponder",
            "Sandbox: curl(9) deny(1) network-outbound /private/var/run/mDNSResponder",
            "Sandbox: git(9) deny(1) mach-lookup com.apple.diagnosticd",
            "Sandbox: git(9) deny(1) mach-lookup com.apple.analyticsd",
        ] {
            assert_eq!(parse_line(noise), None, "{noise}");
        }
        for real in [
            "zsh(9) deny(1) sysctl-read kern.iossupportversion",
            "git(9) deny(1) system-info vfs.disk-space",
            "scutil(9) deny(1) mach-lookup com.apple.SystemConfiguration.configd",
            "pbpaste(9) deny(1) mach-lookup com.apple.pasteboard.1",
            "touch(9) deny(1) file-write-create /x/kern.iossupportversion",
        ] {
            assert_eq!(parse_line(&format!("Sandbox: {real}")), Some(real));
        }
        assert_eq!(parse_line("Sandbox: touch(9) allow file-write-create /x"), None);
    }

    #[test]
    fn caps_and_renders_the_block() {
        let v = Violations::new();
        let watch = v.watch("c", "x", Arc::default(), false);
        for i in 0..53 {
            v.record("c", &format!("line {i}"));
        }
        let lines = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(watch.finish(false));
        assert_eq!(lines.len(), 51);
        assert_eq!(lines[50], "… and 3 more");
        assert_eq!(
            annotate("out", &["a".into(), "b".into()]),
            "out\n<sandbox_violations>\na\nb\n</sandbox_violations>"
        );
        assert_eq!(annotate("out", &[]), "out");
    }
}
