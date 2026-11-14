//! Permission modes as agent definitions and the Agent tool declare them.
//!
//! The engine keeps no mode of its own: the host's permission gate applies
//! modes. A subagent's call carries, in its [`AgentOrigin`](crate::AgentOrigin),
//! what each agent of its chain declared ([`DeclaredMode`]: its definition's
//! `permissionMode` and the spawning call's `mode`); [`resolve_chain`] turns
//! the top-level agent's mode and that chain into the mode the call runs in.
//!
//! The rule, for one link (`parent` = the mode of the agent that started it):
//!
//! - The call's `mode` only narrows: it counts when it ranks at or below the
//!   parent's (`plan` < `default` = `dontAsk` = `bubble` < `acceptEdits` <
//!   `auto` < `bypassPermissions`; `acceptEdits` under `auto` does not count).
//!   It wins over the definition's.
//! - The definition's `permissionMode` counts only while the parent runs in
//!   `default` or `dontAsk`: under `plan`, `acceptEdits`, `auto` or
//!   `bypassPermissions` the subagent keeps its parent's mode. It may then
//!   widen (`acceptEdits`) or narrow (`plan`, `dontAsk`).
//! - `bypassPermissions` (from either) counts only when the session may
//!   bypass permissions at all; otherwise the parent's mode stays.
//! - `bubble` (a fork's) is the parent's mode: prompts go to the parent.
//!
//! Isolation (a worktree) does not change the mode.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    DontAsk,
    BypassPermissions,
    Auto,
    Bubble,
}

impl PermissionMode {
    /// The name as frontmatter and settings spell it.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "default" => Self::Default,
            "acceptEdits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "dontAsk" => Self::DontAsk,
            "bypassPermissions" => Self::BypassPermissions,
            "auto" => Self::Auto,
            "bubble" => Self::Bubble,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
            Self::Auto => "auto",
            Self::Bubble => "bubble",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Default | Self::DontAsk | Self::Bubble => 1,
            Self::AcceptEdits => 2,
            Self::Auto => 3,
            Self::BypassPermissions => 4,
        }
    }
}

/// What one subagent of a chain declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredMode {
    /// Its agent type (`fork` for a fork).
    pub agent_type: Arc<str>,
    /// Its definition's `permissionMode`.
    pub definition: Option<PermissionMode>,
    /// The `mode` of the Agent call that started it.
    pub spawn: Option<PermissionMode>,
}

impl DeclaredMode {
    /// Declares nothing: the subagent runs in its parent's mode.
    pub fn is_empty(&self) -> bool {
        self.definition.is_none() && self.spawn.is_none()
    }
}

/// The mode a subagent runs in, from its parent's and what it declared.
/// `bypass_available`: the session may run in `bypassPermissions` at all.
pub fn resolve_mode(parent: PermissionMode, declared: &DeclaredMode, bypass_available: bool) -> PermissionMode {
    let parent = if parent == PermissionMode::Bubble { PermissionMode::Default } else { parent };
    let spawn = declared.spawn.filter(|m| {
        !(parent == PermissionMode::Auto && *m == PermissionMode::AcceptEdits) && m.rank() <= parent.rank()
    });
    let Some(wanted) = spawn.or(declared.definition) else { return parent };
    let parent_fixed = matches!(
        parent,
        PermissionMode::BypassPermissions | PermissionMode::AcceptEdits | PermissionMode::Auto | PermissionMode::Plan
    );
    if spawn.is_none() && parent_fixed {
        return parent;
    }
    match wanted {
        PermissionMode::Bubble => parent,
        PermissionMode::BypassPermissions if !bypass_available => parent,
        other => other,
    }
}

/// The mode of the last agent of `chain` (top-level agent first, as
/// [`AgentOrigin::modes`](crate::AgentOrigin::modes) holds it) when the
/// top-level agent runs in `root`.
pub fn resolve_chain(root: PermissionMode, chain: &[DeclaredMode], bypass_available: bool) -> PermissionMode {
    chain.iter().fold(root, |parent, declared| resolve_mode(parent, declared, bypass_available))
}

/// Whether any link of `chain` declares `plan`. For a host gate that does
/// not apply declared modes, the engine keeps such a subagent read-only
/// itself (never wider than declared).
pub fn always_plan(chain: &[DeclaredMode]) -> bool {
    chain.iter().any(|d| d.spawn == Some(PermissionMode::Plan) || d.definition == Some(PermissionMode::Plan))
}

#[cfg(test)]
mod tests {
    use super::PermissionMode::*;
    use super::*;

    fn declared(definition: Option<PermissionMode>, spawn: Option<PermissionMode>) -> DeclaredMode {
        DeclaredMode { agent_type: "t".into(), definition, spawn }
    }

    #[test]
    fn definitions_count_only_under_a_default_parent() {
        assert_eq!(resolve_mode(Default, &declared(Some(AcceptEdits), None), false), AcceptEdits);
        assert_eq!(resolve_mode(Default, &declared(Some(Plan), None), false), Plan);
        assert_eq!(resolve_mode(DontAsk, &declared(Some(Plan), None), false), Plan);
        for parent in [BypassPermissions, AcceptEdits, Auto, Plan] {
            assert_eq!(resolve_mode(parent, &declared(Some(Plan), None), true), parent);
            assert_eq!(resolve_mode(parent, &declared(Some(Default), None), true), parent);
        }
        assert_eq!(resolve_mode(Default, &declared(Some(BypassPermissions), None), false), Default);
        assert_eq!(resolve_mode(Default, &declared(Some(BypassPermissions), None), true), BypassPermissions);
        assert_eq!(resolve_mode(Plan, &declared(Some(Bubble), None), true), Plan);
        assert_eq!(resolve_mode(Default, &declared(Some(Bubble), None), true), Default);
    }

    #[test]
    fn the_calls_mode_only_narrows_and_wins() {
        assert_eq!(resolve_mode(BypassPermissions, &declared(Some(AcceptEdits), Some(Plan)), true), Plan);
        assert_eq!(resolve_mode(Default, &declared(None, Some(AcceptEdits)), false), Default);
        assert_eq!(resolve_mode(Auto, &declared(None, Some(AcceptEdits)), true), Auto);
        assert_eq!(resolve_mode(AcceptEdits, &declared(None, Some(Default)), true), Default);
        // A call's mode that does not count leaves the definition's.
        assert_eq!(resolve_mode(Default, &declared(Some(Plan), Some(BypassPermissions)), true), Plan);
    }

    #[test]
    fn chains_fold_from_the_top() {
        let chain = [declared(Some(AcceptEdits), None), declared(Some(Plan), None)];
        // acceptEdits fixes the second link's parent: its plan does not count.
        assert_eq!(resolve_chain(Default, &chain, false), AcceptEdits);
        assert!(always_plan(&chain));
        assert!(!always_plan(&chain[..1]));
    }
}
