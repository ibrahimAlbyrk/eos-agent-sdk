//! Which tools an agent gets and in what order: Claude Code sends its
//! built-in tools sorted by name, then the MCP tools sorted by name, with the
//! rarely used ones deferred behind ToolSearch.

use std::sync::Arc;

use eos_agent_core::{Tool, Toolbox};
use eos_agent_tools_session::{DeferredTool, ToolSearch};

use crate::decorate::Undeferred;
use crate::host::HostTool;

const TOOL_SEARCH: &str = "ToolSearch";

fn is_mcp(tool: &Arc<dyn Tool>) -> bool {
    tool.spec().name().starts_with("mcp__")
}

/// One toolbox from `tools` (any order, without ToolSearch): ToolSearch is
/// built last, over every deferred tool, when there is one. Without
/// `defer`, deferred tools are sent in full and there is no ToolSearch.
pub(crate) fn compose(tools: impl IntoIterator<Item = Arc<dyn Tool>>, defer: bool) -> Toolbox {
    let tools = tools.into_iter().filter(|t| t.spec().name() != TOOL_SEARCH);
    let (mut mcp, mut builtin): (Vec<_>, Vec<_>) =
        if defer { tools.partition(is_mcp) } else { tools.map(Undeferred::wrap).partition(is_mcp) };
    let deferred: Vec<DeferredTool> = builtin
        .iter()
        .chain(&mcp)
        .filter(|t| t.spec().is_deferred())
        .map(|t| DeferredTool::from_spec(t.spec()))
        .collect();
    if !deferred.is_empty() {
        builtin.push(Arc::new(ToolSearch::new(deferred)));
    }
    builtin.sort_by(|a, b| a.spec().name().cmp(b.spec().name()));
    mcp.sort_by(|a, b| a.spec().name().cmp(b.spec().name()));
    builtin.extend(mcp);
    Toolbox::new(builtin)
}

/// The tools of `parent` that `keep` allows, recomposed (ToolSearch then
/// searches only what is left).
pub(crate) fn narrow(parent: &Toolbox, keep: impl Fn(&str) -> bool, defer: bool) -> Toolbox {
    compose(parent.tools().iter().filter(|t| keep(t.spec().name())).cloned(), defer)
}

/// `toolbox` with each tool that `wording` also has (by name) taken from
/// `wording`: the same tools as another model's variant describes them.
pub(crate) fn reword(toolbox: &Toolbox, wording: &[Arc<dyn Tool>]) -> Toolbox {
    let swap = |tool: &Arc<dyn Tool>| {
        let name = tool.spec().name();
        wording.iter().find(|t| t.spec().name() == name).map_or_else(|| Arc::clone(tool), Arc::clone)
    };
    Toolbox::new(toolbox.tools().iter().map(swap).collect())
}

/// Names sent with `defer_loading`, for the session context's announcement.
pub(crate) fn deferred_names(toolbox: &Toolbox) -> Vec<String> {
    toolbox.tools().iter().filter(|t| t.spec().is_deferred()).map(|t| t.spec().name().to_owned()).collect()
}

pub(crate) fn has(toolbox: &Toolbox, name: &str) -> bool {
    toolbox.get(name).is_some()
}

/// What an agent's toolbox is made of besides its directory's tools and its
/// MCP servers'.
#[derive(Clone)]
pub(crate) struct ToolRecipe {
    /// The process-wide tools as the agent's model profile words them.
    pub(crate) base: Arc<[Arc<dyn Tool>]>,
    /// The host's own tools for the agent.
    pub(crate) host: Vec<HostTool>,
    pub(crate) filter: ToolFilter,
    /// Deferred tools wait behind ToolSearch (`false`: all sent in full).
    pub(crate) defer: bool,
    /// Claude Code's full-prompt texts for the directory's tools.
    pub(crate) full_wording: bool,
}

/// Which tools an agent may have: `allow` (when set) lists the only ones it
/// gets, `deny` takes tools away. A name ending in `*` matches by prefix
/// (`mcp__github__*`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct ToolFilter {
    pub(crate) allow: Option<Vec<String>>,
    pub(crate) deny: Vec<String>,
}

impl ToolFilter {
    pub(crate) fn allows(&self, name: &str) -> bool {
        // ToolSearch is rebuilt over whatever deferred tools survive.
        if name == TOOL_SEARCH {
            return true;
        }
        let matches = |pattern: &String| match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => pattern == name,
        };
        self.allow.as_ref().is_none_or(|allow| allow.iter().any(matches)) && !self.deny.iter().any(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_by_name_and_prefix() {
        let filter = ToolFilter { allow: Some(vec!["Read".into(), "mcp__gh__*".into()]), deny: vec![] };
        assert!(filter.allows("Read") && filter.allows("mcp__gh__issues") && !filter.allows("Write"));
        let filter = ToolFilter { allow: None, deny: vec!["Bash".into()] };
        assert!(filter.allows("Read") && !filter.allows("Bash"));
    }
}
