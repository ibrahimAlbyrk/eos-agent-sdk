//! Reads what Claude Code keeps on disk, so an agent sees the same
//! instructions and account facts it would under Claude Code. Plain data
//! out; the host decides how to present it.

mod account;
mod claude_md;
mod markdown;
mod mcp;
mod ordered;
mod settings;
mod transcript;

use std::path::{Path, PathBuf};

pub use account::{GlobalConfig, McpApproval, ProjectMcp, read_global_config};
pub use claude_md::{ClaudeMdFile, MemoryScope, discover_claude_md};
pub use mcp::{McpDocuments, mcp_documents};
pub use ordered::HEADER_ORDER;
pub use settings::{managed_settings, project_settings, user_and_managed_settings, user_settings};
pub use transcript::{ClaudeSession, transcript_path};

/// Where Claude Code's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePaths {
    /// Expands `~/` in CLAUDE.md imports.
    pub home: PathBuf,
    /// `~/.claude` (or `$CLAUDE_CONFIG_DIR`): user CLAUDE.md and rules.
    pub config_dir: PathBuf,
    /// `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`): account and per-project state.
    pub global_config: PathBuf,
    /// The organization-managed folder (`/Library/Application Support/ClaudeCode` on macOS).
    pub managed_dir: PathBuf,
}

impl ClaudePaths {
    /// Claude Code's default layout under `home`.
    pub fn for_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            config_dir: home.join(".claude"),
            global_config: home.join(".claude.json"),
            managed_dir: PathBuf::from("/Library/Application Support/ClaudeCode"),
        }
    }

    /// From `$HOME`, honoring `$CLAUDE_CONFIG_DIR` like Claude Code does.
    pub fn from_env() -> Option<Self> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        let mut paths = Self::for_home(Path::new(&home));
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
            paths.config_dir = PathBuf::from(&dir);
            paths.global_config = PathBuf::from(dir).join(".claude.json");
        }
        Some(paths)
    }
}

/// Claude Code's name for a project's folder under `~/.claude/projects`
/// (`hD`): every UTF-16 unit other than an ASCII letter or digit becomes
/// `-`; a name over 200 units is cut there and given a `-<hash>` suffix
/// (`Math.abs` of the Java-style string hash, in base 36).
pub fn project_dir_name(path: &Path) -> String {
    const MAX: usize = 200;
    let text = path.to_string_lossy();
    let units: Vec<u16> = text.encode_utf16().collect();
    let name: String = units
        .iter()
        .map(|&u| match char::from_u32(u32::from(u)) {
            Some(c) if c.is_ascii_alphanumeric() => c,
            _ => '-',
        })
        .collect();
    if name.len() <= MAX {
        return name;
    }
    let hash = units.iter().fold(0i32, |h, &u| h.wrapping_shl(5).wrapping_sub(h).wrapping_add(i32::from(u)));
    format!("{}-{}", &name[..MAX], base36(i64::from(hash).unsigned_abs()))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folder_names_follow_claude_code() {
        assert_eq!(project_dir_name(Path::new("/Users/u/my.repo")), "-Users-u-my-repo");
        assert_eq!(project_dir_name(Path::new("/tmp/ü😀")), "-tmp----");
        // From node, with Claude Code 2.1.319's `hD` and `pne`.
        let long = format!("/{}", "a".repeat(250));
        assert_eq!(project_dir_name(Path::new(&long)), format!("-{}-feo44x", "a".repeat(199)));
    }
}
