//! Where Claude Code's MCP server configs live for a session: the user and
//! local entries of `~/.claude.json` and the project's `.mcp.json`, of which
//! only approved servers load.

use std::path::Path;

use serde_json::{Map, Value};

use crate::account::{GlobalConfig, McpApproval, RawApproval};

/// The config documents by scope, each `{"mcpServers": {...}}` or the bare map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpDocuments {
    /// `~/.claude.json` top-level `mcpServers`.
    pub user: Option<Value>,
    /// The nearest `.mcp.json` from `cwd` up, approved servers only.
    pub project: Option<Value>,
    /// `~/.claude.json` `projects.<cwd>.mcpServers`.
    pub local: Option<Value>,
}

/// Blocking I/O: one `stat` per directory up from `cwd`, plus the files found.
pub fn mcp_documents(global: &GlobalConfig, cwd: &Path) -> McpDocuments {
    let project_state = global.project_mcp.get(cwd);
    let mut approval = project_state.map(|p| p.approval.clone()).unwrap_or_default();
    for settings in [".claude/settings.json", ".claude/settings.local.json"] {
        if let Some(raw) = read_json(&cwd.join(settings)).and_then(|v| serde_json::from_value::<RawApproval>(v).ok()) {
            approval.merge(raw.into());
        }
    }
    let project =
        cwd.ancestors().find_map(|dir| read_json(&dir.join(".mcp.json"))).map(|doc| approved(&doc, &approval));
    McpDocuments {
        user: global.mcp_servers.clone(),
        project: project.filter(|doc| doc.as_object().is_some_and(|m| !m.is_empty())),
        local: project_state.and_then(|p| p.servers.clone()),
    }
}

/// The bare server map of `doc`, keeping only servers `approval` allows.
fn approved(doc: &Value, approval: &McpApproval) -> Value {
    let servers = doc.get("mcpServers").unwrap_or(doc);
    let kept: Map<String, Value> = servers
        .as_object()
        .map(|m| m.iter().filter(|(name, _)| approval.allows(name)).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    Value::Object(kept)
}

fn read_json(path: &Path) -> Option<Value> {
    crate::ordered::parse(std::str::from_utf8(&std::fs::read(path).ok()?).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ProjectMcp;

    #[test]
    fn project_servers_need_approval() {
        let dir = std::env::temp_dir().join(format!("eos-compat-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).expect("dir");
        let doc = r#"{"mcpServers":{"a":{"command":"x"},"b":{"command":"y"},"c":{"command":"z"}}}"#;
        std::fs::write(dir.join(".mcp.json"), doc).expect("write");
        std::fs::write(dir.join(".claude/settings.local.json"), r#"{"enabledMcpjsonServers":["c"]}"#).expect("write");
        let mut global =
            GlobalConfig { mcp_servers: Some(serde_json::json!({"u": {"command": "u"}})), ..Default::default() };
        let approval =
            McpApproval { enabled: vec!["a".into(), "b".into()], disabled: vec!["b".into()], ..Default::default() };
        let local = Some(serde_json::json!({"l": {"command": "l"}}));
        global.project_mcp.insert(dir.clone(), ProjectMcp { servers: local.clone(), approval });

        let docs = mcp_documents(&global, &dir);
        let names: Vec<&String> = docs.project.as_ref().and_then(Value::as_object).expect("project").keys().collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(docs.local, local);
        assert!(docs.user.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
