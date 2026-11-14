//! The shell snapshot: the user's functions, aliases, options and PATH,
//! captured once per shell by running their rc file in a login shell, then
//! sourced by every command instead of starting a login shell each time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::sync::{OnceCell, Semaphore};

use crate::process::{GroupGuard, quote};

const CREATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Snapshots by shell, created on first use and shared by every agent.
/// Dropping it deletes the files it wrote.
#[derive(Debug)]
pub struct ShellSnapshots {
    dir: PathBuf,
    by_shell: Mutex<HashMap<PathBuf, Arc<OnceCell<Option<PathBuf>>>>>,
}

impl ShellSnapshots {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, by_shell: Mutex::default() }
    }

    /// The snapshot for `shell`, or `None` when it could not be made (commands
    /// then run as login shells).
    pub async fn get(&self, shell: &Path, budget: &Semaphore) -> Option<PathBuf> {
        let cell = {
            let mut map = self.by_shell.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(map.entry(shell.to_path_buf()).or_default())
        };
        cell.get_or_init(|| self.create(shell, budget)).await.clone()
    }

    async fn create(&self, shell: &Path, budget: &Semaphore) -> Option<PathBuf> {
        let kind = shell_kind(shell);
        tokio::fs::create_dir_all(&self.dir).await.ok()?;
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let rand: String = std::iter::repeat_with(fastrand::alphanumeric).take(6).collect();
        let file = self.dir.join(format!("snapshot-{kind}-{millis}-{}.sh", rand.to_ascii_lowercase()));
        let rc = std::env::var_os("HOME").map(|h| Path::new(&h).join(rc_name(kind))).filter(|p| p.is_file());
        let script = script(kind, &file, rc.as_deref());

        let _permit = budget.acquire().await.ok()?;
        let mut child = Command::new(shell)
            .args(["-c", "-l", &script])
            .env("SHELL", shell)
            .env("GIT_EDITOR", "true")
            .env("CLAUDECODE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .ok()?;
        let _guard = GroupGuard::new(child.id());
        let status = tokio::time::timeout(CREATE_TIMEOUT, child.wait()).await.ok()?.ok()?;
        (status.success() && file.is_file()).then_some(file)
    }
}

impl Drop for ShellSnapshots {
    fn drop(&mut self) {
        let map = self.by_shell.get_mut().unwrap_or_else(PoisonError::into_inner);
        for cell in map.values() {
            if let Some(Some(path)) = cell.get() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn shell_kind(shell: &Path) -> &'static str {
    let name = shell.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.contains("zsh") {
        "zsh"
    } else if name.contains("bash") {
        "bash"
    } else {
        "sh"
    }
}

fn rc_name(kind: &str) -> &'static str {
    match kind {
        "zsh" => ".zshrc",
        "bash" => ".bashrc",
        _ => ".profile",
    }
}

/// The script the login shell runs to write the snapshot to `file`.
fn script(kind: &str, file: &Path, rc: Option<&Path>) -> String {
    let mut s = format!("SNAPSHOT_FILE={}\n", quote(&file.to_string_lossy()));
    if let Some(rc) = rc {
        s += &format!("source {} < /dev/null\n", quote(&rc.to_string_lossy()));
    }
    // Aliases are frozen into function bodies when those are defined, so the
    // snapshot clears them before redefining anything.
    s += "echo '# Shell snapshot' >| \"$SNAPSHOT_FILE\"\n";
    s += "echo 'unalias -a 2>/dev/null || true' >> \"$SNAPSHOT_FILE\"\n";
    match (kind, rc.is_some()) {
        ("zsh", true) => s += ZSH_STATE,
        ("bash", true) => s += BASH_STATE,
        ("zsh", false) => {}
        _ => s += "echo 'shopt -s expand_aliases' >> \"$SNAPSHOT_FILE\"\n",
    }
    s += "builtin printf 'export PATH=%q\\n' \"$PATH\" >> \"$SNAPSHOT_FILE\"\n";
    s += "[ -f \"$SNAPSHOT_FILE\" ] || exit 1\n";
    s
}

/// Functions (minus `_completion` ones, keeping `__helpers`), options, aliases.
const ZSH_STATE: &str = r#"typeset -f > /dev/null 2>&1
typeset +f | grep -vE '^_[^_]' | while read -r fn_name; do
  typeset -f -- "$fn_name" >> "$SNAPSHOT_FILE"
done
setopt | sed 's/^/setopt /' | head -n 1000 >> "$SNAPSHOT_FILE"
alias | sed -e '1001,$d' -e 's/^alias //' -e 's/^/alias -- /' >> "$SNAPSHOT_FILE"
"#;

/// shopt first: function bodies using extglob syntax only parse with it on.
/// One eval per function, so a body that no longer parses drops only itself.
const BASH_STATE: &str = r#"shopt -p >> "$SNAPSHOT_FILE"
declare -F | while read -r _ _ fn_name; do
  case "$fn_name" in _[!_]*) continue ;; esac
  printf 'eval %q > /dev/null 2>&1\n' "$(declare -f -- "$fn_name")" >> "$SNAPSHOT_FILE"
done
set -o | awk '$2 == "on" {print "set -o " $1}' >> "$SNAPSHOT_FILE"
echo 'shopt -s expand_aliases' >> "$SNAPSHOT_FILE"
alias -p | sed -e '1001,$d' -e 's/^alias -- /alias /' -e 's/^alias /alias -- /' >> "$SNAPSHOT_FILE"
"#;
