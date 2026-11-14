//! The paths Claude Code 2.1.319 keeps sandboxed commands from writing (its
//! `J0` on macOS, with its feature flags at their defaults): its settings,
//! state and instruction files in its config directory and in every folder
//! from the working directory up to the home folder, `.mcp.json` files up to
//! the root, the git files that run code (hooks, config) or redirect a
//! repository, its global config, credential and device-key files, the
//! Anthropic profile store, its folders in its temp root, the plugin
//! folders the environment names, and sandbox-runtime's names protected at
//! any depth. Its feature gates are read as `_t(name, {safe: true})`, on
//! unless the server turns them off, so what they add is here too
//! (`~/.claude/ssh`, the plugin-tool staging folder, git `remotes`/`branches`,
//! pinned `info` folders, nested `config.lock` files). Names that need a
//! command-line flag or a variable a plain install leaves unset are left out.
//! The folders Claude Code makes before protecting them are made the same
//! way; it creates no placeholder files on macOS (only its Linux sandbox
//! needs them), so neither does this.

use std::path::{Path, PathBuf};

use super::SandboxDirs;

/// Claude Code's always-protected names in a working directory and below
/// it (sandbox-runtime's `Zwr`).
const PROTECTED_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
];
/// …and directories (`eEr`).
const PROTECTED_DIRS: &[&str] = &[".vscode", ".idea", ".claude/commands", ".claude/agents"];

/// Claude Code's own state in its config directory (`J0`'s fixed list).
const CONFIG_STATE: &[&str] = &[
    "shell-snapshots",
    "session-env",
    "plugins",
    "hooks",
    "skills",
    "workflows",
    "commands",
    "agents",
    "routines",
    "rules",
    "output-styles",
    "scheduled_tasks.json",
    "launch.json",
    "CLAUDE.md",
    "projects",
    "daemon.json",
    "policy-limits.json",
    "policy-limits.json.signature.json",
    "policy-limits.json.signature-iat.json",
    "policy-limits.json.stamp.json",
    "backups",
];
/// More of it, protected after the plugin folders (`J0`, `Yuo` and the
/// remote-settings files with their signatures).
const CONFIG_MORE: &[&str] = &[
    "mcp-skill-archives",
    "mcp-discovery-cache",
    "shares",
    "todos",
    "statsig",
    "logs",
    "access-audit",
    "remote-settings-helper-consent",
    "remote-settings-consent.json",
    "remote-settings.json",
    "remote-settings.json.signature.json",
    "remote-settings.json.signature-iat.json",
    "remote-settings.json.synced-at",
];

/// A protected entry: one path (and everything below it), or a name at any
/// depth under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Protected {
    Path(PathBuf),
    /// `<root>/**/<tail>`; a tail ending in `/**` means the directory's contents.
    Below {
        root: PathBuf,
        tail: String,
    },
    /// A glob, spelled as sandbox-runtime reads one.
    Glob(String),
}

/// What a command may not touch beyond the settings' own lists.
#[derive(Debug, Default)]
pub(crate) struct Fence {
    pub write: Vec<Protected>,
    /// Claude Code's own folders it hides from reading (`ide`, `seed-admin`, `bridge-spawn`).
    pub read: Vec<PathBuf>,
    /// Folders Claude Code makes writable for the working directory: the
    /// main repository's git directory of a linked worktree.
    pub writable: Vec<PathBuf>,
    /// The first so many of `write` are Claude Code's own (`J0`); the rest
    /// are sandbox-runtime's names, which it adds to the profile only, so
    /// the prompt does not list them.
    pub own: usize,
}

/// Claude Code's protected set for an agent in `cwd` with working
/// directories `roots`; every path given is already resolved.
pub(crate) fn fence(cwd: &Path, roots: &[PathBuf], dirs: &SandboxDirs) -> Fence {
    let mut f = Fence::default();
    let home = dirs.home.as_deref();
    let config = dirs.config_dir.clone().or_else(|| home.map(|h| h.join(".claude")));
    let mut path = |p: PathBuf| f.write.push(Protected::Path(p));

    // Each settings tier's file, the managed drop-ins, the project's skills and hooks.
    if let Some(config) = &config {
        path(config.join("settings.json"));
    }
    path(cwd.join(".claude/settings.json"));
    path(cwd.join(".claude/settings.local.json"));
    if let Some(managed) = &dirs.managed_dir {
        path(managed.join("managed-settings.json"));
        path(managed.join("managed-settings.d"));
    }
    path(cwd.join(".claude/skills"));
    path(cwd.join(".claude/hooks"));
    if let Some(config) = &config {
        path(config.join("local"));
        // Its atomic-write staging folders (`Y$`).
        path(cwd.join(".claude/.cc-writes"));
        path(config.join(".cc-writes"));
        path(config.join("jobs"));
        // Made (owner only) before it is protected, as Claude Code makes it.
        make_private(&config.join("seed-admin"));
        path(config.join("seed-admin"));
        path(config.join("daemon"));
    }
    if let Some(home) = home {
        path(home.join(".claude/bridge-spawn"));
    }
    // `S7e`: Claude Code's Bash edit-diff folder in its temp root, made
    // and also hidden from reading.
    let temp_root = claude_temp_root();
    let edit_diff = temp_root.join("bash-edit-diff");
    make_private(&edit_diff);
    path(edit_diff.clone());
    if let Some(config) = &config {
        for name in CONFIG_STATE.iter().chain(&["loop.md", "cowork_plugins"]) {
            path(config.join(name));
        }
        make_private(&config.join("dev-mods"));
        path(config.join("dev-mods"));
    }
    // `C$` (`bot_protected_tengu_graceful_dream`, on unless the server
    // turns it off): the staged MCP-call folder in the temp root, made by `lb`.
    let staging = temp_root.join("plugin-tool-staging");
    make_private(&staging);
    path(staging);
    // Plugin folders the environment names, and where installed plugins' commands come from.
    for dir in plugin_dirs(cwd, config.as_deref(), |k| std::env::var_os(k)) {
        path(dir);
    }
    if let Some(config) = &config {
        for name in CONFIG_MORE {
            path(config.join(name));
        }
        path(config.join("state"));
    }
    if let Some(home) = home
        && config.as_deref() != Some(&home.join(".claude"))
    {
        path(home.join(".claude/state"));
    }
    // `A$` (`tengu_resilient_adleman`, on by default): its ssh binaries, made first.
    if let Some(home) = home {
        let ssh = home.join(".claude/ssh");
        make_private(&ssh);
        path(ssh);
    }
    // `LDt`: the global config file (and its OAuth-variant names), the
    // credentials and device keys.
    if let Some(home) = home {
        let global = global_config_dir(config.as_deref(), home);
        for name in
            [".claude.json", ".claude-staging-oauth.json", ".claude-local-oauth.json", ".claude-custom-oauth.json"]
        {
            path(global.join(name));
        }
    }
    if let Some(config) = &config {
        path(config.join(".config.json"));
        path(config.join(".credentials.json"));
    }
    if let Some(home) = home {
        path(home.join(".claude/.device-keys.json"));
    }
    // `izn`: the Anthropic profile store, made first.
    if let Some(store) = profile_store(home, |k| std::env::var_os(k)) {
        make_private(&store);
        path(store);
    }
    if let Some(config) = &config {
        f.read.push(config.join("seed-admin"));
    }
    if let Some(home) = home {
        f.read.push(home.join(".claude/bridge-spawn"));
    }
    f.read.push(edit_diff);
    for ide in ide_dirs(config.as_deref(), home) {
        path(ide.clone());
        f.read.push(ide);
    }

    // The working directory's Claude Code files, then every folder up to
    // the home folder, `.mcp.json` up to the root, and the home folder's.
    project_files(cwd, &mut f.write);
    let mut chain = cwd.to_path_buf();
    loop {
        folder_files(&chain, &mut f.write);
        if Some(chain.as_path()) == home {
            break;
        }
        match chain.parent() {
            Some(parent) => chain = parent.to_path_buf(),
            None => break,
        }
    }
    mcp_up_to_root(cwd, &mut f.write);
    if let Some(home) = home {
        folder_files(home, &mut f.write);
        mcp_up_to_root(home, &mut f.write);
    }
    let linked = linked_worktree(cwd);
    if let Some(main) = linked.as_ref().and_then(|common| common.parent()) {
        folder_files(main, &mut f.write);
    }

    // Git: what would make the working directory a repository, or change one.
    git_files(cwd, &mut f.write);
    if let Some(root) = git_root(cwd).filter(|r| r != cwd) {
        dot_git(&root, &mut f.write, false);
    }
    if let Some(common) = linked {
        git_dir(&common, &mut f.write, true);
        f.writable.push(common);
    }
    config_lock_globs(cwd, &mut f.write);
    for root in roots.iter().filter(|r| r.as_path() != cwd) {
        f.write.push(Protected::Path(root.join(".mcp.json")));
    }

    dedup(&mut f.write);
    f.own = f.write.len();

    // sandbox-runtime's names, at any depth under each working directory.
    for root in roots {
        for name in PROTECTED_FILES {
            f.write.push(Protected::Path(root.join(name)));
            f.write.push(Protected::Below { root: root.clone(), tail: (*name).to_owned() });
        }
        for dir in PROTECTED_DIRS {
            f.write.push(Protected::Path(root.join(dir)));
            f.write.push(Protected::Below { root: root.clone(), tail: format!("{dir}/**") });
        }
        f.write.push(Protected::Path(root.join(".git/hooks")));
        f.write.push(Protected::Below { root: root.clone(), tail: ".git/hooks/**".into() });
        f.write.push(Protected::Path(root.join(".git/config")));
        f.write.push(Protected::Below { root: root.clone(), tail: ".git/config".into() });
    }
    dedup(&mut f.write);
    f
}

/// Where Claude Code keeps `.claude.json` (`PFr`): `$CLAUDE_CONFIG_DIR`,
/// else the home folder; the config directory when an old non-empty
/// `.config.json` is there.
fn global_config_dir(config: Option<&Path>, home: &Path) -> PathBuf {
    if let Some(config) = config
        && std::fs::metadata(config.join(".config.json")).is_ok_and(|m| m.is_file() && m.len() > 0)
    {
        return config.to_path_buf();
    }
    std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()).map_or_else(|| home.to_path_buf(), PathBuf::from)
}

/// `RF`: `$ANTHROPIC_CONFIG_DIR`, `$XDG_CONFIG_HOME/anthropic` or
/// `~/.config/anthropic`.
fn profile_store(home: Option<&Path>, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let set = |k: &str| var(k).filter(|v| !v.to_string_lossy().trim().is_empty()).map(PathBuf::from);
    set("ANTHROPIC_CONFIG_DIR")
        .or_else(|| set("XDG_CONFIG_HOME").map(|x| x.join("anthropic")))
        .or_else(|| home.map(|h| h.join(".config/anthropic")))
}

/// Claude Code's temp root (`Fm`): `$CLAUDE_CODE_TMPDIR` or `/tmp`, then
/// `claude-<uid>`, resolved (`/private/tmp/claude-501`).
pub(crate) fn claude_temp_root() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CODE_TMPDIR").filter(|v| !v.is_empty()).unwrap_or_else(|| "/tmp".into());
    let root = PathBuf::from(base).join(format!("claude-{}", uid()));
    std::fs::canonicalize(&root)
        .or_else(|_| {
            root.parent()
                .map_or_else(|| Ok(root.clone()), std::fs::canonicalize)
                .map(|p| p.join(root.file_name().unwrap_or_default()))
        })
        .unwrap_or(root)
}

#[allow(unsafe_code)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `mwr`: the IDE lock folders (also hidden from reading).
fn ide_dirs(config: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = config.map(|c| c.join("ide")).into_iter().collect();
    if let Some(home) = home
        && config != Some(&home.join(".claude"))
    {
        out.push(home.join(".claude/ide"));
    }
    out
}

/// `Zi`: a project's launch, workflow, routine, output-style and schedule
/// files and its `.mcp.json`.
fn project_files(dir: &Path, out: &mut Vec<Protected>) {
    let claude = dir.join(".claude");
    for name in ["launch.json", "workflows", "routines", "output-styles", "scheduled_tasks.json", "loop.md"] {
        out.push(Protected::Path(claude.join(name)));
    }
    out.push(Protected::Path(dir.join(".mcp.json")));
}

/// `tt`: [`project_files`] plus the folder's skills, commands, agents, hooks
/// and settings.
fn folder_files(dir: &Path, out: &mut Vec<Protected>) {
    project_files(dir, out);
    let claude = dir.join(".claude");
    for name in ["skills", "commands", "agents", "hooks", "settings.json", "settings.local.json"] {
        out.push(Protected::Path(claude.join(name)));
    }
}

fn mcp_up_to_root(dir: &Path, out: &mut Vec<Protected>) {
    for dir in dir.ancestors() {
        out.push(Protected::Path(dir.join(".mcp.json")));
    }
}

/// A working directory's own git files: those that would make it look like
/// a bare repository (`HEAD`, `objects`, `refs`), a `config` file there, and
/// its `.git` (which may not be created where there is none).
fn git_files(dir: &Path, out: &mut Vec<Protected>) {
    let head = dir.join("HEAD");
    let bare_head = head.is_file();
    // A HEAD file, or a link to nothing, makes `hooks` and `config` count whatever they are.
    let head_like = bare_head || std::fs::symlink_metadata(&head).is_ok_and(|m| m.file_type().is_symlink());
    for name in ["HEAD", "objects", "refs"] {
        out.push(Protected::Path(dir.join(name)));
    }
    for name in ["hooks", "config"] {
        let p = dir.join(name);
        if std::fs::symlink_metadata(&p).is_err() {
            continue;
        }
        let config_file = name == "config" && std::fs::metadata(&p).is_ok_and(|m| m.is_file());
        if !head_like && !config_file {
            // A folder named so (not a repository's): only a `.git` inside it.
            if std::fs::metadata(&p).is_ok_and(|m| m.is_dir()) && p.join(".git").exists() {
                out.push(Protected::Path(p.join(".git")));
            }
            continue;
        }
        out.push(Protected::Path(p));
        if config_file {
            out.push(Protected::Path(dir.join("config.lock")));
        }
    }
    if bare_head && dir.join("objects").is_dir() && dir.join("refs").is_dir() {
        git_dir(dir, out, false);
    }
    dot_git(dir, out, true);
}

/// `ah`: `<dir>/.git` — a file (a worktree's pointer) as is, a folder's
/// risky files (also through a link), one that cannot be looked at
/// (`tengu_mighty_thimble`, on by default) as is, and when absent the name
/// itself if `absent` says so.
fn dot_git(dir: &Path, out: &mut Vec<Protected>, absent: bool) {
    let git = dir.join(".git");
    match std::fs::symlink_metadata(&git) {
        Err(_) => {
            if absent {
                out.push(Protected::Path(git));
            }
        }
        Ok(m) if m.is_file() => out.push(Protected::Path(git)),
        Ok(m) if m.is_dir() => git_dir(&git, out, true),
        Ok(_) => match std::fs::metadata(&git) {
            Ok(m) if m.is_dir() => git_dir(&git, out, true),
            Ok(_) => {}
            Err(e) if !matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => out.push(Protected::Path(git)),
            Err(_) => {}
        },
    }
}

/// `bo`: the files of a git directory that run code or redirect it.
/// `own` is a `.git` folder (not a working directory that is itself a bare
/// repository): Claude Code makes its missing `info` folders before pinning them.
fn git_dir(git: &Path, out: &mut Vec<Protected>, own: bool) {
    for name in ["hooks", "config", "config.lock", "config.worktree", "config.worktree.lock"] {
        out.push(Protected::Path(git.join(name)));
    }
    // `N$` (`tengu_serene_flurry`, on by default): the legacy remote
    // folders, made where missing; a git directory where one cannot be made
    // for another reason is protected whole.
    let mut odd = false;
    for name in ["remotes", "branches"] {
        odd |= make_in(git, name);
        out.push(Protected::Path(git.join(name)));
    }
    if odd {
        out.push(Protected::Path(git.to_path_buf()));
    }
    out.push(Protected::Path(git.join("commondir")));
    info_pins(git, own, out);
    for name in ["alternates", "http-alternates"] {
        out.push(Protected::Path(git.join("objects/info").join(name)));
    }
    let worktrees = git.join("worktrees");
    let entries = match std::fs::read_dir(&worktrees) {
        Ok(entries) => entries,
        Err(e) => {
            // `I$` (`tengu_quiet_medlar`, on by default): a folder that cannot be listed is protected whole.
            let gone = [libc::ENOENT, libc::ENOTDIR, libc::ELOOP, libc::ENAMETOOLONG];
            if !e.raw_os_error().is_some_and(|c| gone.contains(&c)) {
                out.push(Protected::Path(worktrees));
            }
            return;
        }
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()))
        .map(|e| e.path())
        .collect();
    names.sort();
    for wt in names {
        for name in ["config.worktree", "config.worktree.lock", "commondir"] {
            out.push(Protected::Path(wt.join(name)));
        }
    }
}

/// `p$`: makes `<dir>/<name>` when `dir` is a folder (not a link); true
/// when that fails for a reason other than it existing or `dir` missing.
fn make_in(dir: &Path, name: &str) -> bool {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    match std::fs::create_dir(dir.join(name)) {
        Ok(()) => false,
        Err(e) => !matches!(e.raw_os_error(), Some(libc::EEXIST | libc::ENOENT | libc::ENOTDIR)),
    }
}

/// The name Claude Code reserves in a folder it pins against replacement (`xce`).
const NO_REPLACE: &str = ".claude-sandbox-noreplace";

/// `k$` (`tengu_riveted_lantern`, on by default): the git directory's
/// `info`, `objects/info` and `objects/info/commit-graphs` folders cannot be
/// replaced, since a write denied below a folder keeps it from being moved
/// or removed. Missing ones of a `.git` folder are made first.
fn info_pins(git: &Path, own: bool, out: &mut Vec<Protected>) {
    let objects = git.join("objects");
    let objects_info = objects.join("info");
    let pins = [
        (git.join("info"), vec![git.to_path_buf()]),
        (objects_info.clone(), vec![git.to_path_buf(), objects.clone()]),
        (objects_info.join("commit-graphs"), vec![git.to_path_buf(), objects, objects_info]),
    ];
    for (folder, chain) in pins {
        let mut skip = false;
        let mut unreadable = false;
        for step in chain.iter().chain(std::iter::once(&folder)) {
            match std::fs::symlink_metadata(step) {
                Err(e) if step == &folder && e.raw_os_error() == Some(libc::ENOENT) => {
                    if own && std::fs::symlink_metadata(folder.parent().unwrap_or(git)).is_ok_and(|m| m.is_dir()) {
                        let _ = std::fs::create_dir(&folder);
                    }
                    break;
                }
                Err(e) => {
                    // Missing on the way, or not a folder: not pinned. Unreadable: read-only whole.
                    if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) {
                        skip = true;
                    } else {
                        unreadable = true;
                    }
                    break;
                }
                Ok(m) if !m.is_dir() => {
                    skip = true;
                    break;
                }
                Ok(_) => {}
            }
        }
        if skip {
            continue;
        }
        let reserved = folder.join(NO_REPLACE);
        if unreadable {
            out.push(Protected::Path(folder));
            out.push(Protected::Path(reserved));
            continue;
        }
        match std::fs::symlink_metadata(&reserved) {
            Ok(m) if !m.is_dir() && !m.is_file() => out.push(Protected::Path(folder)),
            Err(e) if e.raw_os_error() != Some(libc::ENOENT) => out.push(Protected::Path(folder)),
            _ => out.push(Protected::Path(reserved)),
        }
    }
}

/// `T$` (`tengu_golden_pebble`, on by default, macOS): a nested
/// repository's `config.lock` anywhere under the working directory, in
/// every case and with the ligature and dotless-i spellings APFS folds
/// together; left out when the working directory's name holds `*` or `?`.
fn config_lock_globs(cwd: &Path, out: &mut Vec<Protected>) {
    let text = cwd.to_string_lossy();
    if text.contains(['*', '?']) {
        return;
    }
    let root = format!("{}/**/.[gG][iI][tT]", text.replace('[', "[[]"));
    for fi in ["[fF][iI]", "\u{FB01}", "[fF]\u{131}"] {
        for k in ["[kK]", "\u{212A}"] {
            out.push(Protected::Glob(format!("{root}/[cC][oO][nN]{fi}[gG].[lL][oO][cC]{k}")));
        }
    }
}

/// Plugin folders Claude Code protects beyond its config directory's own
/// (`lO`, `dU`, `wEs`): `$CLAUDE_CODE_PLUGIN_CACHE_DIR`, each folder of
/// `$CLAUDE_CODE_PLUGIN_SEED_DIR`, and the folders installed plugins'
/// commands come from (`sourceProducerPath`, `previousProducerPaths` in
/// each of those folders' `installed_plugins.json`); none that holds the
/// working directory.
fn plugin_dirs(cwd: &Path, config: Option<&Path>, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let resolve = |raw: &str| -> PathBuf {
        let expanded = match raw.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                var("HOME").map(|h| PathBuf::from(h).join(rest.trim_start_matches('/')))
            }
            _ => None,
        };
        normalize(&cwd.join(expanded.unwrap_or_else(|| PathBuf::from(raw))))
    };
    let holds_cwd = |dir: &Path| cwd.starts_with(dir);
    let set = |k: &str| var(k).map(|v| v.to_string_lossy().into_owned()).filter(|v| !v.is_empty());
    let own: Vec<PathBuf> = config.map(|c| vec![c.join("plugins"), c.join("cowork_plugins")]).unwrap_or_default();
    let cache = set("CLAUDE_CODE_PLUGIN_CACHE_DIR").map(|v| resolve(&v));
    let seeds: Vec<PathBuf> = set("CLAUDE_CODE_PLUGIN_SEED_DIR")
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(resolve).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Some(cache) = &cache
        && !own.contains(cache)
        && !holds_cwd(cache)
    {
        out.push(cache.clone());
    }
    out.extend(seeds.iter().filter(|s| !holds_cwd(s)).cloned());
    let mut scanned: Vec<PathBuf> = Vec::new();
    for dir in own.iter().chain(cache.iter()).chain(&seeds) {
        if scanned.contains(dir) {
            continue;
        }
        scanned.push(dir.clone());
        for producer in producer_paths(&dir.join("installed_plugins.json")) {
            let producer = normalize(&producer);
            if !holds_cwd(&producer) && !out.contains(&producer) {
                out.push(producer);
            }
        }
    }
    out
}

/// The absolute `sourceProducerPath` and `previousProducerPaths` of every
/// entry of an `installed_plugins.json` (at most 4 MiB).
fn producer_paths(file: &Path) -> Vec<PathBuf> {
    let Ok(meta) = std::fs::metadata(file) else { return Vec::new() };
    if !meta.is_file() || meta.len() > 4 * 1024 * 1024 {
        return Vec::new();
    }
    let Some(doc) = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
        return Vec::new();
    };
    let Some(plugins) = doc.get("plugins").and_then(serde_json::Value::as_object) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in plugins.values().filter_map(serde_json::Value::as_array).flatten() {
        let Some(entry) = entry.as_object() else { continue };
        let source = entry.get("sourceProducerPath").and_then(serde_json::Value::as_str);
        let previous = entry.get("previousProducerPaths").and_then(serde_json::Value::as_array);
        for p in source.into_iter().chain(previous.into_iter().flatten().filter_map(serde_json::Value::as_str)) {
            if p.starts_with('/') {
                out.push(PathBuf::from(p));
            }
        }
    }
    out
}

/// `.` and `..` folded away without touching the disk (`path.resolve`).
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

/// `mkdir -p` with owner-only permissions; failures are ignored, as Claude Code ignores them.
fn make_private(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
}

/// The nearest folder at or above `dir` holding a `.git`.
fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// `Awr`: for a linked worktree (`.git` is a file naming
/// `<repo>/.git/worktrees/<name>`), the main repository's git directory.
fn linked_worktree(dir: &Path) -> Option<PathBuf> {
    let git = dir.join(".git");
    if !std::fs::symlink_metadata(&git).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&git).ok()?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let target = dir.join(target);
    let worktrees = target.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common = worktrees.parent()?;
    common.file_name()?.to_string_lossy().ends_with(".git").then(|| std::fs::canonicalize(common).ok())?
}

fn dedup(items: &mut Vec<Protected>) {
    let mut seen = Vec::with_capacity(items.len());
    items.retain(|p| {
        if seen.contains(p) {
            return false;
        }
        seen.push(p.clone());
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> SandboxDirs {
        SandboxDirs {
            home: Some("/Users/u".into()),
            config_dir: Some("/Users/u/.claude".into()),
            managed_dir: Some("/Library/Application Support/ClaudeCode".into()),
        }
    }

    fn has(f: &Fence, p: &str) -> bool {
        f.write.contains(&Protected::Path(p.into()))
    }

    #[test]
    fn claude_codes_state_and_project_files() {
        let f = fence(Path::new("/Users/u/w/p"), &["/Users/u/w/p".into()], &dirs());
        for p in [
            "/Users/u/.claude/settings.json",
            "/Users/u/w/p/.claude/settings.local.json",
            "/Library/Application Support/ClaudeCode/managed-settings.d",
            "/Users/u/.claude/projects",
            "/Users/u/.claude/shell-snapshots",
            "/Users/u/.claude/remote-settings.json.signature.json",
            "/Users/u/.claude/state",
            "/Users/u/.claude/ide",
            "/Users/u/.claude/bridge-spawn",
            "/Users/u/w/p/.claude/workflows",
            "/Users/u/w/.claude/settings.json",
            "/Users/u/.claude/commands",
            "/Users/u/.mcp.json",
            "/.mcp.json",
            "/Users/u/w/p/HEAD",
            "/Users/u/w/p/.git",
            "/Users/u/.claude/dev-mods",
            "/Users/u/.claude/policy-limits.json.stamp.json",
            "/Users/u/.claude/remote-settings.json.synced-at",
            "/Users/u/.claude/.credentials.json",
            "/Users/u/.claude/.config.json",
            "/Users/u/.claude/.device-keys.json",
        ] {
            assert!(has(&f, p), "{p} protected");
        }
        assert!(!has(&f, "/Users/.claude/settings.json"), "the folder walk stops at the home folder");
        assert!(f.read.contains(&PathBuf::from("/Users/u/.claude/ide")));
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_none() {
            assert!(has(&f, "/Users/u/.claude.json") && has(&f, "/Users/u/.claude-custom-oauth.json"));
        }
        let edit_diff = claude_temp_root().join("bash-edit-diff");
        assert!(has(&f, &edit_diff.to_string_lossy()) && f.read.contains(&edit_diff), "hidden and protected");
        // Behind gates that are on unless the server turns them off.
        assert!(has(&f, "/Users/u/.claude/ssh"));
        assert!(has(&f, &claude_temp_root().join("plugin-tool-staging").to_string_lossy()));
        let lock = "/Users/u/w/p/**/.[gG][iI][tT]/[cC][oO][nN]\u{FB01}[gG].[lL][oO][cC]\u{212A}";
        assert!(f.write.contains(&Protected::Glob(lock.into())), "{:?}", f.write);
        assert!(f.write.contains(&Protected::Below { root: "/Users/u/w/p".into(), tail: ".zshrc".into() }));
    }

    #[test]
    fn plugin_folders_from_the_environment() {
        let base =
            std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-plug-{}", fastrand::u64(..)));
        let config = base.join("config");
        std::fs::create_dir_all(config.join("plugins")).expect("plugins");
        let installed = json_installed("/opt/producer", &["/opt/old", "relative/skipped"]);
        std::fs::write(config.join("plugins/installed_plugins.json"), installed).expect("write");
        let cwd = base.join("w");
        let seeds = format!("/seed/a::{}", base.display());
        let vars = [("CLAUDE_CODE_PLUGIN_CACHE_DIR", "cache"), ("CLAUDE_CODE_PLUGIN_SEED_DIR", seeds.as_str())];
        let var = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(v));
        let dirs = plugin_dirs(&cwd, Some(&config), var);
        // The second seed holds the working directory, so it stays writable.
        let want: Vec<PathBuf> =
            [cwd.join("cache"), "/seed/a".into(), "/opt/producer".into(), "/opt/old".into()].into_iter().collect();
        assert_eq!(dirs, want);
        assert!(plugin_dirs(&cwd, Some(&config), |_| None).len() == 2, "installed producers without any variable");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn json_installed(source: &str, previous: &[&str]) -> String {
        serde_json::json!({"version": 2, "plugins": {"p@m": [{"scope": "user", "sourceProducerPath": source, "previousProducerPaths": previous}]}})
            .to_string()
    }

    #[test]
    fn the_profile_store_follows_its_variables() {
        let home = Path::new("/Users/u");
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(v))
        };
        assert_eq!(profile_store(Some(home), env(&[])), Some(PathBuf::from("/Users/u/.config/anthropic")));
        assert_eq!(profile_store(Some(home), env(&[("XDG_CONFIG_HOME", "/x")])), Some(PathBuf::from("/x/anthropic")));
        let both = env(&[("XDG_CONFIG_HOME", "/x"), ("ANTHROPIC_CONFIG_DIR", "/a")]);
        assert_eq!(profile_store(Some(home), both), Some(PathBuf::from("/a")));
    }

    #[test]
    fn git_directories_and_linked_worktrees() {
        let base =
            std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-fence-{}", fastrand::u64(..)));
        let repo = base.join("repo");
        let wt = base.join("wt");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).expect("git dir");
        std::fs::create_dir_all(repo.join(".git/objects")).expect("objects");
        std::fs::create_dir_all(&wt).expect("wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display())).expect("w");
        std::fs::create_dir_all(wt.join("hooks/.git")).expect("hooks");
        let f = fence(&wt, std::slice::from_ref(&wt), &dirs());
        assert!(f.write.contains(&Protected::Path(wt.join("hooks/.git"))), "a folder's own repository");
        assert!(!f.write.contains(&Protected::Path(wt.join("hooks"))), "not a repository's hooks without HEAD");
        let git = repo.join(".git");
        assert_eq!(f.writable, vec![git.clone()], "a linked worktree may write its repository's git directory");
        for p in [git.join("hooks"), git.join("config"), git.join("worktrees/wt/commondir"), wt.join(".git")] {
            assert!(f.write.contains(&Protected::Path(p.clone())), "{} protected", p.display());
        }
        assert!(f.write.contains(&Protected::Path(repo.join(".claude/settings.json"))), "the main checkout's files");
        // The legacy remote folders and the pinned info folders are made, as Claude Code makes them.
        for made in ["remotes", "branches", "info", "objects/info/commit-graphs"] {
            assert!(git.join(made).is_dir(), "{made} made");
        }
        for p in [git.join("remotes"), git.join("objects/info/commit-graphs").join(NO_REPLACE)] {
            assert!(f.write.contains(&Protected::Path(p.clone())), "{} protected", p.display());
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
