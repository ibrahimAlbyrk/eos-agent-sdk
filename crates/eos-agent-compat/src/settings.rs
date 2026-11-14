//! Claude Code's settings files: the user's `~/.claude/settings.json`, the
//! organization's managed settings, and a project's
//! `.claude/settings.json` and `.claude/settings.local.json`.

use std::path::Path;

use serde_json::Value;

use crate::ClaudePaths;

/// The user's and the managed settings documents, lowest precedence first
/// (user, then `managed-settings.json`, then its `managed-settings.d/*.json`
/// drop-ins in name order). Missing or unreadable files are left out.
/// Blocking I/O.
pub fn user_and_managed_settings(paths: &ClaudePaths) -> Vec<Value> {
    let mut docs: Vec<Value> = user_settings(paths).into_iter().collect();
    docs.extend(managed_settings(paths));
    docs
}

/// The user's `settings.json` in Claude Code's config directory. Blocking I/O.
pub fn user_settings(paths: &ClaudePaths) -> Option<Value> {
    read(&paths.config_dir.join("settings.json"))
}

/// The managed settings documents, lowest precedence first:
/// `managed-settings.json`, then its `managed-settings.d/*.json` drop-ins in
/// name order. Blocking I/O.
pub fn managed_settings(paths: &ClaudePaths) -> Vec<Value> {
    let mut docs = Vec::new();
    docs.extend(read(&paths.managed_dir.join("managed-settings.json")));
    if let Ok(entries) = std::fs::read_dir(paths.managed_dir.join("managed-settings.d")) {
        let mut files: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        docs.extend(files.iter().filter_map(|p| read(p)));
    }
    docs
}

/// A project's shared and local settings (`<cwd>/.claude/settings.json`,
/// `<cwd>/.claude/settings.local.json`), as Claude Code reads them for the
/// directory it runs in. Blocking I/O.
pub fn project_settings(cwd: &Path) -> (Option<Value>, Option<Value>) {
    (read(&cwd.join(".claude/settings.json")), read(&cwd.join(".claude/settings.local.json")))
}

fn read(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_first_then_managed() {
        let home = std::env::temp_dir().join(format!("eos-compat-settings-{}", std::process::id()));
        let mut paths = ClaudePaths::for_home(&home);
        paths.managed_dir = home.join("managed");
        std::fs::create_dir_all(paths.config_dir.clone()).expect("dir");
        std::fs::create_dir_all(paths.managed_dir.join("managed-settings.d")).expect("dir");
        std::fs::write(paths.config_dir.join("settings.json"), r#"{"sandbox":{"enabled":true}}"#).expect("write");
        std::fs::write(paths.managed_dir.join("managed-settings.json"), r#"{"sandbox":{"enabled":false}}"#).expect("w");
        std::fs::write(paths.managed_dir.join("managed-settings.d/b.json"), r#"{"b":1}"#).expect("write");
        std::fs::write(paths.managed_dir.join("managed-settings.d/a.json"), r#"{"a":1}"#).expect("write");
        std::fs::write(paths.managed_dir.join("managed-settings.d/c.txt"), "not json").expect("write");
        let docs = user_and_managed_settings(&paths);
        assert_eq!(docs.len(), 4);
        assert_eq!(docs[0]["sandbox"]["enabled"], true);
        assert_eq!(docs[1]["sandbox"]["enabled"], false);
        assert_eq!((docs[2]["a"].as_i64(), docs[3]["b"].as_i64()), (Some(1), Some(1)));
        std::fs::create_dir_all(home.join("p/.claude")).expect("dir");
        std::fs::write(home.join("p/.claude/settings.local.json"), r#"{"sandbox":{"enabled":true}}"#).expect("w");
        let (shared, local) = project_settings(&home.join("p"));
        assert!(shared.is_none() && local.is_some_and(|l| l["sandbox"]["enabled"] == true));
        let _ = std::fs::remove_dir_all(&home);
    }
}
