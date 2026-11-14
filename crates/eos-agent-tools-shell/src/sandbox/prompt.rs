//! Claude Code's `## Bash command sandbox` section (`VFn`), which leads the
//! session's system message while the sandbox is on. Paths are the agent's
//! working directories as given (the start folder shows as `.`) and its
//! temp directory shows as `$TMPDIR`, so agents of one project share it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::paths::{self, expand};
use super::{SandboxConfig, SandboxDirs, protect};

/// Lists longer than this are cut in the prompt (Claude Code's `W6`).
const LIST_MAX: usize = 50;

const BOUNDARY: &str = "The sandbox marks out what this session was given: the directories listed below, the network \
destinations the task involves, and the credentials the user supplied for it. Treat that as the boundary even where a \
limit below is not enforced. Commands can reach more than that \u{2014} credentials and keys elsewhere on this machine, \
the user's other projects and configuration, sockets that control this machine or other workloads, cloud metadata \
endpoints \u{2014} but being reachable does not make them provided; those are the user's, not the task's, unless the \
user's request calls for them. If the task cannot be finished with what you were given, do what you can and tell the \
user plainly what is missing instead of finding another way to it; that report is a complete answer.";

pub(crate) fn section(config: &SandboxConfig, cwd: &Path, roots: &[PathBuf], dirs: &SandboxDirs) -> String {
    let unsandboxed_ok = config.allow_unsandboxed_commands();
    let mut config_lines = vec![format!("Filesystem: {}", escape(&filesystem(config, cwd, roots, dirs)))];
    let mut network = serde_json::Map::new();
    if !config.denied_domains().is_empty() {
        network.insert("deniedHosts".into(), list(dedup(config.denied_domains().to_vec())));
    }
    // Set (even empty) counts, as in Claude Code (`...b&&{allowUnixSockets}`, `if(H)`).
    if let Some(sockets) = config.network.allow_unix_sockets.as_ref() {
        network.insert("allowUnixSockets".into(), list(dedup(sockets.clone())));
    }
    if !network.is_empty() {
        config_lines.push(format!("Network: {}", escape(&Value::Object(network).to_string())));
    }
    if let Some(ignore) = config.ignore_violations.as_ref() {
        config_lines.push(format!("Ignored violations: {}", escape(&json!(ignore).to_string())));
    }

    let mut items: Vec<Item> = Vec::new();
    if unsandboxed_ok {
        items.push(Item::One(
            "You should always default to running commands within the sandbox. Do NOT attempt to set \
             `dangerouslyDisableSandbox: true` unless:"
                .into(),
        ));
        items.push(Item::Many(vec![
            "The user *explicitly* asks you to bypass sandbox".into(),
            "A specific command just failed and you see evidence of sandbox restrictions causing the failure. Note \
             that commands can fail for many reasons unrelated to the sandbox (missing files, wrong arguments, network \
             issues, etc.)."
                .into(),
        ]));
        items.push(Item::One("Evidence of sandbox-caused failures includes:".into()));
        items.push(Item::Many(vec![
            "\"Operation not permitted\" errors for file/network operations".into(),
            "Access denied to specific paths outside allowed directories".into(),
            "Network connection failures to non-whitelisted hosts".into(),
            "Unix socket connection errors".into(),
        ]));
        items.push(Item::One("When you see evidence of sandbox-caused failure:".into()));
        items.push(Item::Many(vec![
            "If the `<sandbox_violations>` block names a denied host, re-run the command with that host in its \
             `allowed_domains` (auto mode); otherwise retry with `dangerouslyDisableSandbox: true` directly rather \
             than asking in prose first \u{2014} the retry itself goes through the permission gate (a user prompt, or \
             the auto-mode classifier when auto mode is active)"
                .into(),
            "Briefly explain what sandbox restriction likely caused the failure. Be sure to mention that the user can \
             use the `/sandbox` command to manage restrictions."
                .into(),
        ]));
        items.push(Item::One(
            "A sandbox denial on a credential, a file or a host that the task does not involve is the boundary above \
             at work: tell the user rather than retrying with `dangerouslyDisableSandbox: true`."
                .into(),
        ));
        items.push(Item::One(
            "Treat each command you execute with `dangerouslyDisableSandbox: true` individually. Even if you have \
             recently run a command with this setting, you should default to running future commands within the \
             sandbox."
                .into(),
        ));
        items.push(Item::One(
            "Do not suggest adding sensitive paths like ~/.bashrc, ~/.zshrc, ~/.ssh/*, or credential files to the \
             sandbox allowlist."
                .into(),
        ));
    } else {
        items.push(Item::One(
            "The `dangerouslyDisableSandbox` parameter is disabled in this session's configuration; setting it does \
             not take a command out of the sandbox."
                .into(),
        ));
        items.push(Item::One(
            "If a command the task needs fails on a sandbox restriction, tell the user which restriction it hit; \
             changing the sandbox settings is their decision, not yours."
                .into(),
        ));
    }
    items.push(Item::One(
        "Network egress goes through a filtering proxy. Attempt requests and read the error rather than predicting \
         whether a host is reachable; denied connections are reported in a `<sandbox_violations>` block explaining \
         the reason. In auto mode, list the hosts a command needs beyond that in its `allowed_domains` \u{2014} a \
         denied host named in the block goes there on the re-run."
            .into(),
    ));
    if !config.allow_local_binding() {
        items.push(Item::One(if unsandboxed_ok {
            "If a command fails to bind or listen on a local port with \"Operation not permitted\" (EPERM), local port \
             binding is off in this sandbox. Treat it as the sandbox-caused failure described above, and tell the user \
             that `sandbox.network.allowLocalBinding: true` in their settings (it applies without a restart) allows it \
             without leaving the sandbox."
                .into()
        } else {
            "If a command fails to bind or listen on a local port with \"Operation not permitted\" (EPERM), local port \
             binding is off in this sandbox. Tell the user they can allow it with \
             `sandbox.network.allowLocalBinding: true` in their settings (it applies without a restart); changing \
             sandbox settings is their decision, not yours."
                .into()
        }));
    }
    if config.allow_apple_events != Some(true) {
        items.push(Item::One(
            "Opening an app, file or URL with `open`, or scripting another app with `osascript`, is blocked inside \
             this sandbox (macOS Launch Services and Apple Events are off) and fails with errors such as -10822 \
             (kLSServerCommunicationErr), -54, -600 or \"LSOpenURLsWithRole() failed\". That failure is the sandbox, \
             not a problem with what you built; do not retry it with `dangerouslyDisableSandbox: true`. Tell the user \
             the sandbox blocked it and give them the exact command to run themselves."
                .into(),
        ));
    }
    items.push(Item::One(
        "For temporary files, always use the `$TMPDIR` environment variable. TMPDIR is automatically set to the \
         correct sandbox-writable directory in sandbox mode. Do NOT use `/tmp` directly - use `$TMPDIR` instead."
            .into(),
    ));

    let mut out = vec![
        "## Bash command sandbox".to_owned(),
        "By default, Bash commands run inside an OS-level sandbox (macOS Seatbelt) applied to each command \
         separately, not to the session as a whole; how it is configured in this session is described below."
            .to_owned(),
        String::new(),
        BOUNDARY.to_owned(),
        String::new(),
        "How the sandbox is configured in this session:".to_owned(),
        config_lines.join("\n"),
        String::new(),
    ];
    for item in items {
        match item {
            Item::One(text) => out.push(format!(" - {text}")),
            Item::Many(list) => out.extend(list.into_iter().map(|t| format!("  - {t}"))),
        }
    }
    out.join("\n")
}

enum Item {
    One(String),
    Many(Vec<String>),
}

/// `{"read":{"denyOnly":[…],"allowWithinDeny":[…]},"write":{"allowOnly":[…],"denyWithinAllow":[…]}}`.
fn filesystem(config: &SandboxConfig, cwd: &Path, roots: &[PathBuf], dirs: &SandboxDirs) -> String {
    let fs = &config.filesystem;
    let home = dirs.home.as_deref();
    let fence = protect::fence(cwd, roots, dirs);
    // Globs too, a trailing `/**` dropped (sandbox-runtime's `m_`).
    let shown = |list: &Option<Vec<String>>| -> Vec<String> {
        list.iter().flatten().filter_map(|p| expand(p, cwd, home)).map(|p| p.to_string_lossy().into_owned()).collect()
    };
    // Claude Code's order (`J0`): its defaults, the working directory as
    // `.`, its temp folders (shown as `$TMPDIR`), a linked worktree's git
    // directory, the other working directories, then `allowWrite`.
    let mut allow_only: Vec<String> =
        paths::default_writable(config, cwd, home).iter().map(|p| p.to_string_lossy().into_owned()).collect();
    allow_only.push(".".to_owned());
    allow_only.push("$TMPDIR".to_owned());
    allow_only.extend(fence.writable.iter().map(|p| p.to_string_lossy().into_owned()));
    allow_only.extend(roots.iter().filter(|r| r.as_path() != cwd).map(|r| r.to_string_lossy().into_owned()));
    allow_only.extend(shown(&fs.allow_write));
    // Claude Code's own denies (sandbox-runtime adds its names to the profile only).
    let mut deny_within: Vec<String> = fence.write[..fence.own].iter().map(paths::Protected::shown).collect();
    for path in fs.deny_write.iter().flatten() {
        if let Some(p) = expand(path, cwd, home) {
            deny_within.push(p.to_string_lossy().into_owned());
        }
    }
    deny_within.extend(twins(&deny_within));
    let mut deny_read: Vec<String> = fence.read.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    deny_read.extend(shown(&fs.deny_read));
    deny_read.extend(twins(&deny_read));
    deny_read.extend(shown(&config.credentials.as_ref().map(|c| c.denied_files())));
    let value = json!({
        "read": {"denyOnly": list(dedup(deny_read)), "allowWithinDeny": list(dedup(shown(&fs.allow_read)))},
        "write": {"allowOnly": list(dedup(allow_only)), "denyWithinAllow": list(dedup(deny_within))},
    });
    // Claude Code's key order: read before write, denyOnly before allowWithinDeny.
    format!(
        "{{\"read\":{{\"denyOnly\":{},\"allowWithinDeny\":{}}},\"write\":{{\"allowOnly\":{},\"denyWithinAllow\":{}}}}}",
        value["read"]["denyOnly"],
        value["read"]["allowWithinDeny"],
        value["write"]["allowOnly"],
        value["write"]["denyWithinAllow"]
    )
}

/// Claude Code's real-path twins on macOS (`Bv`): a deny written through a
/// symlink (`/tmp/…`, a linked home or project folder) also gets its real
/// spelling and its real parent's, listed after the others. Globs keep
/// their spelling; a twin that is an ancestor of the deny's folder, or that
/// sits on a special file, is not listed (`jce`, `qp`).
fn twins(denies: &[String]) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for deny in denies {
        if deny.contains(['*', '?', '[']) || !deny.starts_with('/') {
            continue;
        }
        let path = Path::new(deny);
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { continue };
        if special(path) {
            continue;
        }
        let real_parent = paths::real(parent);
        let mut seen: Vec<String> = Vec::new();
        for twin in [paths::real(path), real_parent.join(name)] {
            let twin: String = twin
                .to_string_lossy()
                .chars()
                .map(|c| if matches!(c, '*' | '?' | '[' | ']' | '"' | '\\' | '\0'..='\x1f' | '\x7f') { '?' } else { c })
                .collect();
            let above = |dir: &Path| format!("{}/", dir.display()).starts_with(&format!("{twin}/"));
            if twin == *deny || seen.contains(&twin) || above(parent) || above(&real_parent) {
                continue;
            }
            seen.push(twin);
        }
        out.extend(seen);
    }
    out
}

/// A device or other special file on the way (Claude Code gives it no twin).
fn special(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::metadata(path).is_ok_and(|m| {
        let t = m.file_type();
        t.is_block_device() || t.is_char_device() || t.is_fifo() || t.is_socket()
    })
}

/// What Claude Code does to each JSON value it shows (`nf`): `<` and `>`
/// as HTML entities, and the C1 controls and line/paragraph separators
/// (which JSON leaves as they are) as numeric ones.
fn escape(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("&#{};", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

fn list(mut items: Vec<String>) -> Value {
    if items.len() > LIST_MAX {
        let more = items.len() - LIST_MAX;
        items.truncate(LIST_MAX);
        items.push(format!("... and {more} more (truncated for prompt size)"));
    }
    json!(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The section Claude Code 2.1.319 sent (`claude -p` with `sandbox.enabled`),
    /// its Filesystem line (Claude Code's own paths) left out.
    const CAPTURED: &str = include_str!("../../tests/fixtures/sandbox-section.txt");

    fn dirs() -> SandboxDirs {
        SandboxDirs { home: Some("/Users/u".into()), config_dir: Some("/Users/u/.claude".into()), managed_dir: None }
    }

    #[test]
    fn matches_claude_codes_section() {
        let cfg: SandboxConfig = serde_json::from_value(json!({"enabled": true})).expect("config");
        let text = section(&cfg, Path::new("/w/p"), &[PathBuf::from("/w/p")], &dirs());
        let (head, rest) = text.split_once("\nFilesystem: ").expect("filesystem line");
        let (fs, tail) = rest.split_once('\n').expect("tail");
        assert_eq!(format!("{head}\nFilesystem: <FS>\n{tail}"), CAPTURED.trim_end_matches('\n'));
        let fs: Value = serde_json::from_str(fs).expect("json");
        let allow = fs["write"]["allowOnly"].as_array().expect("list");
        assert_eq!(allow[0], "/dev/stdout");
        assert_eq!(allow[allow.len() - 2], ".");
        assert_eq!(allow[allow.len() - 1], "$TMPDIR");
        let more: SandboxConfig =
            serde_json::from_value(json!({"enabled": true, "filesystem": {"allowWrite": ["/out"]}})).expect("config");
        let text = section(&more, Path::new("/w/p"), &[PathBuf::from("/w/p"), PathBuf::from("/w/q")], &dirs());
        assert!(
            text.contains("\"/Users/u/.claude/debug\",\".\",\"$TMPDIR\",\"/w/q\",\"/out\"],\"denyWithinAllow\""),
            "Claude Code's order: `.`, its temp folders, other directories, allowWrite: {text}"
        );
        assert_eq!(fs["write"]["denyWithinAllow"][0], "/Users/u/.claude/settings.json");
        let edit_diff = protect::claude_temp_root().join("bash-edit-diff").to_string_lossy().into_owned();
        let own = ["/Users/u/.claude/seed-admin", "/Users/u/.claude/bridge-spawn", &edit_diff, "/Users/u/.claude/ide"];
        assert_eq!(fs["read"]["denyOnly"], json!(own), "Claude Code's own folders, in its order");
        assert!(edit_diff.starts_with("/private/tmp/claude-"), "{edit_diff}");
    }

    #[test]
    fn options_change_the_text() {
        let cfg: SandboxConfig = serde_json::from_value(json!({
            "enabled": true, "allowUnsandboxedCommands": false, "allowAppleEvents": true,
            "network": {"allowLocalBinding": true, "deniedDomains": ["evil.com"]},
            "filesystem": {"denyRead": ["~/.ssh"]},
            "credentials": {"files": [{"path": "/Users/u/.netrc", "mode": "mask"}]},
        }))
        .expect("config");
        let text = section(&cfg, Path::new("/w"), &[PathBuf::from("/w")], &dirs());
        assert!(text.contains("\nNetwork: {\"deniedHosts\":[\"evil.com\"]}\n"));
        assert!(text.contains("/.claude/ide\",\"/Users/u/.ssh\",\"/Users/u/.netrc\"]"), "{text}");
        assert!(text.contains(" - The `dangerouslyDisableSandbox` parameter is disabled"));
        assert!(!text.contains("local port"));
        assert!(!text.contains("osascript"));
    }

    #[test]
    fn filesystem_line_as_claude_code_writes_it() {
        let cfg: SandboxConfig = serde_json::from_value(json!({
            "enabled": true,
            "filesystem": {
                "denyRead": ["~/.claude", "/w/**/*.pem"],
                "allowRead": ["~/.claude/debug"],
                "allowWrite": ["/out/**", "/w/gen/*.txt", "/w/a<b>"],
            },
            "network": {"allowUnixSockets": []},
            "ignoreViolations": {},
            "credentials": {"files": [{"path": "~/.npm", "mode": "deny"}]},
        }))
        .expect("config");
        let text = section(&cfg, Path::new("/w"), &[PathBuf::from("/w")], &dirs());
        let fs = text.split("\nFilesystem: ").nth(1).and_then(|t| t.lines().next()).expect("line");
        assert!(fs.contains("\"/Users/u/.claude\",\"/w/**/*.pem\",\"/Users/u/.npm\"]"), "globs shown: {fs}");
        assert!(fs.contains("\"allowWithinDeny\":[\"/Users/u/.claude/debug\"]"), "{fs}");
        assert!(
            fs.contains("\"/private/tmp/claude\",\"/Users/u/.claude/debug\",\".\""),
            "~/.npm/_logs is under a denied credential file; ~/.claude/debug is read again by allowRead: {fs}"
        );
        assert!(fs.contains("\"$TMPDIR\",\"/out\",\"/w/gen/*.txt\",\"/w/a&lt;b&gt;\"]"), "{fs}");
        assert!(text.contains("\nNetwork: {\"allowUnixSockets\":[]}\n"), "set, even empty");
        assert!(text.contains("\nIgnored violations: {}\n"), "set, even empty");

        let cfg: SandboxConfig =
            serde_json::from_value(json!({"enabled": true, "filesystem": {"denyRead": ["~/.claude"]}}))
                .expect("config");
        let text = section(&cfg, Path::new("/w"), &[PathBuf::from("/w")], &dirs());
        assert!(!text.contains("/.claude/debug"), "denied and not read again: {text}");
        assert!(text.contains("/Users/u/.npm/_logs"));
        assert!(!text.contains("Network:") && !text.contains("Ignored violations:"));
    }

    /// sandbox-runtime's names reach the profile only; a deny written
    /// through a symlink gets its real spellings after the others (`Bv`).
    #[test]
    fn runtime_names_left_out_and_real_path_twins_added() {
        let base =
            std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-twin-{}", fastrand::u64(..)));
        std::fs::create_dir_all(base.join("real")).expect("dir");
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("link");
        let deny = base.join("link/secret").to_string_lossy().into_owned();
        let real = base.join("real/secret").to_string_lossy().into_owned();
        assert_eq!(twins(std::slice::from_ref(&deny)), std::slice::from_ref(&real), "its real spelling");
        assert_eq!(twins(std::slice::from_ref(&real)), Vec::<String>::new(), "a real path has no twin");
        assert!(twins(&[format!("{}/link/*.pem", base.display())]).is_empty(), "a glob keeps its spelling");
        let fence = protect::fence(Path::new("/w"), &[PathBuf::from("/w")], &dirs());
        let runtime = |p: &paths::Protected| matches!(p, paths::Protected::Below { .. });
        assert!(!fence.write[..fence.own].iter().any(runtime), "Claude Code's own list");
        assert!(fence.write[fence.own..].iter().any(runtime), "sandbox-runtime's names, in the profile");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn values_are_escaped_as_claude_code_escapes_them() {
        assert_eq!(escape("[\"a<b>\u{85}\u{2028}\"]"), "[\"a&lt;b&gt;&#133;&#8232;\"]");
    }
}
