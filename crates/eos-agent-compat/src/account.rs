use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What Claude Code's global config says about the signed-in account and
/// about projects, limited to what the prompts use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalConfig {
    /// The signed-in claude.ai account's email (`oauthAccount.emailAddress`),
    /// the value Claude Code shows as `# userEmail`.
    pub email: Option<String>,
    /// The signed-in account's id (`oauthAccount.accountUuid`).
    pub account_uuid: Option<String>,
    /// This installation's id (top-level `userID`), sent as the device id.
    pub device_id: Option<String>,
    /// Projects whose CLAUDE.md may import files from outside the project
    /// (`projects.<dir>.hasClaudeMdExternalIncludesApproved`).
    pub external_imports_approved: Vec<PathBuf>,
    /// User-scope MCP servers (top-level `mcpServers`).
    pub mcp_servers: Option<serde_json::Value>,
    /// Per-project MCP state, by project directory.
    pub project_mcp: HashMap<PathBuf, ProjectMcp>,
}

/// A project's MCP entries in `~/.claude.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectMcp {
    /// Local-scope servers (`projects.<dir>.mcpServers`).
    pub servers: Option<serde_json::Value>,
    /// `.mcp.json` servers the user approved or turned off.
    pub approval: McpApproval,
}

/// Which `.mcp.json` servers may load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpApproval {
    pub enable_all: bool,
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
}

impl McpApproval {
    pub fn allows(&self, server: &str) -> bool {
        !self.disabled.iter().any(|s| s == server) && (self.enable_all || self.enabled.iter().any(|s| s == server))
    }

    /// Adds another source's answers (settings files say the same things).
    pub fn merge(&mut self, other: McpApproval) {
        self.enable_all |= other.enable_all;
        self.enabled.extend(other.enabled);
        self.disabled.extend(other.disabled);
    }
}

impl GlobalConfig {
    pub fn allows_external_imports(&self, project: &Path) -> bool {
        self.external_imports_approved.iter().any(|p| p == project)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    #[serde(default)]
    oauth_account: Option<Account>,
    #[serde(default, rename = "userID")]
    user_id: Option<String>,
    #[serde(default)]
    projects: HashMap<String, Project>,
    #[serde(default)]
    mcp_servers: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    #[serde(default)]
    email_address: Option<String>,
    #[serde(default)]
    account_uuid: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    #[serde(default)]
    has_claude_md_external_includes_approved: bool,
    #[serde(default)]
    mcp_servers: Option<Box<serde_json::value::RawValue>>,
    #[serde(flatten)]
    approval: RawApproval,
}

/// `.mcp.json` approval keys, as `~/.claude.json` projects and settings files spell them.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawApproval {
    #[serde(default)]
    enable_all_project_mcp_servers: bool,
    #[serde(default)]
    enabled_mcpjson_servers: Vec<String>,
    #[serde(default)]
    disabled_mcpjson_servers: Vec<String>,
}

impl From<RawApproval> for McpApproval {
    fn from(raw: RawApproval) -> Self {
        Self {
            enable_all: raw.enable_all_project_mcp_servers,
            enabled: raw.enabled_mcpjson_servers,
            disabled: raw.disabled_mcpjson_servers,
        }
    }
}

/// Reads `~/.claude.json`. A missing file is an empty config; the file is
/// large (Claude Code keeps per-project history there), so read it once per
/// process and share the result.
pub fn read_global_config(path: &Path) -> std::io::Result<GlobalConfig> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GlobalConfig::default()),
        Err(e) => return Err(e),
    };
    let raw: Raw = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let mut approved = Vec::new();
    let mut project_mcp = HashMap::new();
    for (dir, project) in raw.projects {
        let dir = PathBuf::from(dir);
        if project.has_claude_md_external_includes_approved {
            approved.push(dir.clone());
        }
        let servers = project.mcp_servers.and_then(|raw| crate::ordered::parse(raw.get()));
        let mcp = ProjectMcp { servers, approval: project.approval.into() };
        if mcp != ProjectMcp::default() {
            project_mcp.insert(dir, mcp);
        }
    }
    approved.sort();
    let (email, account_uuid) = raw.oauth_account.map_or((None, None), |a| (a.email_address, a.account_uuid));
    Ok(GlobalConfig {
        email: email.filter(|e| !e.is_empty()),
        account_uuid: account_uuid.filter(|a| !a.is_empty()),
        device_id: raw.user_id.filter(|d| !d.is_empty()),
        external_imports_approved: approved,
        mcp_servers: raw.mcp_servers.and_then(|raw| crate::ordered::parse(raw.get())),
        project_mcp,
    })
}
