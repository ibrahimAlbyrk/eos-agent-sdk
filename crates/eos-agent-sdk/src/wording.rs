//! Claude Code's per-model tool wordings: the classic Bash line for models
//! without a prompt bundle of their own, and the full-prompt texts for
//! models without `lean_prompt`.

use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, StoredMessage, Tool, ToolContext, ToolOutput, ToolReview, ToolSpec};
use serde_json::value::RawValue;

/// Claude Code's line in the Bash description for models without a prompt
/// bundle of their own (Haiku 5.5): use the dedicated tools instead.
const BASH_DEDICATED_TOOLS: &str = "- IMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or \
`echo` commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your \
task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user.";

/// `tools` as Claude Code describes them to models that get its classic tool
/// texts: the same tools, Bash with one more line after its third.
pub(crate) fn classic(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .map(|tool| {
            if tool.spec().name() != "Bash" {
                return Arc::clone(tool);
            }
            Redescribed::wrap(Arc::clone(tool), |text| {
                let mut lines: Vec<&str> = text.split('\n').collect();
                lines.insert(3.min(lines.len()), BASH_DEDICATED_TOOLS);
                lines.join("\n")
            })
        })
        .collect()
}

/// `tools` with Claude Code's texts for models that get its full prompt
/// (no `lean_prompt`): the tools whose text depends on the model (its `b0`)
/// worded as it words them there, Bash for a session with Glob and Grep.
pub(crate) fn full_tools(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .map(|tool| {
            let text = match tool.spec().name() {
                "Agent" => return Redescribed::wrap(Arc::clone(tool), |_| full_agent(tool.as_ref())),
                "AskUserQuestion" => include_str!("../prompts/full/AskUserQuestion.md"),
                "Bash" => include_str!("../prompts/full/Bash.md"),
                "Edit" => include_str!("../prompts/full/Edit.md"),
                "Glob" => include_str!("../prompts/full/Glob.md"),
                "Grep" => include_str!("../prompts/full/Grep.md"),
                "Read" => include_str!("../prompts/full/Read.md"),
                "WebSearch" => include_str!("../prompts/full/WebSearch.md"),
                "Write" => include_str!("../prompts/full/Write.md"),
                "WebFetch" => return Redescribed::wrap(Arc::clone(tool), full_web_fetch),
                _ => return Arc::clone(tool),
            };
            Redescribed::wrap(Arc::clone(tool), |_| text.to_owned())
        })
        .collect()
}

/// Claude Code's full Agent text; without background agents (the tool's
/// schema has no `run_in_background`), as it words it then.
fn full_agent(tool: &dyn Tool) -> String {
    const FULL: &str = include_str!("../prompts/full/Agent.md");
    let def: serde_json::Value = serde_json::from_slice(tool.spec().json()).unwrap_or_default();
    if def["input_schema"]["properties"].get("run_in_background").is_some() {
        return FULL.to_owned();
    }
    let background: Vec<&str> = FULL
        .lines()
        .filter(|l| {
            l.starts_with("- Agents run in the background by default.")
                || l.starts_with("- **Foreground vs background**")
                || l.starts_with("- **Don't race**")
        })
        .collect();
    let mut text = FULL.replace(
        "- When the agent is done, its final report is not visible to the user.",
        "- When the agent is done, it will return a single message back to you. The result returned by the agent is \
         not visible to the user.",
    );
    for line in background {
        text = text.replace(&format!("{line}\n"), "");
    }
    text
}

/// Claude Code's full WebFetch text with what the tool's own text says: the
/// artifact links line (with the Artifact tool) and how long pages stay
/// cached (either wording, so a text worded twice stays the same).
fn full_web_fetch(text: &str) -> String {
    let between = |open: &str, close: &str| text.split_once(open).and_then(|(_, rest)| rest.split_once(close));
    let ttl = between("Responses are cached for ", " per URL.")
        .or_else(|| between("(entries expire after ", ")"))
        .map_or("15 minutes", |(ttl, _)| ttl);
    let links = if text.contains("claude.ai artifact links") {
        include_str!("../prompts/full/WebFetchArtifactLinks.md")
    } else {
        ""
    };
    include_str!("../prompts/full/WebFetch.md").replace("{artifact_links}", links).replace("{cache_ttl}", ttl)
}

/// `inner` with another description.
struct Redescribed {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

impl Redescribed {
    fn wrap(inner: Arc<dyn Tool>, describe: impl Fn(&str) -> String) -> Arc<dyn Tool> {
        let def: serde_json::Value = serde_json::from_slice(inner.spec().json()).unwrap_or_default();
        let description = describe(def["description"].as_str().unwrap_or_default());
        let mut spec = ToolSpec::new(inner.spec().name(), &description, def["input_schema"].clone());
        if inner.spec().is_deferred() {
            spec = spec.deferred();
        }
        if let Some(hint) = inner.spec().search_hint() {
            spec = spec.with_search_hint(hint);
        }
        Arc::new(Self { inner, spec })
    }
}

impl std::fmt::Debug for Redescribed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Redescribed").field(&self.spec.name()).finish()
    }
}

impl Tool for Redescribed {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        self.inner.runs_sandboxed(input, extensions)
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        self.inner.observable_input(input, extensions)
    }

    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        self.inner.review(input, ctx)
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        self.inner.call(input, ctx)
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        self.inner.restore(history, extensions)
    }
}

#[cfg(test)]
mod tests {
    use super::full_web_fetch;

    #[test]
    fn full_web_fetch_keeps_the_tools_facts_and_is_stable() {
        let lean = "Fetches a URL.\n\n- Fails on authenticated/private URLs. claude.ai artifact links (x) are \
published artifacts.\n- Responses are cached for 2 minutes per URL.";
        let full = full_web_fetch(lean);
        assert!(full.contains("(entries expire after 2 minutes)"));
        assert!(
            full.contains("\n- claude.ai artifact links (claude.ai/artifact/{id} or claude.ai/code/artifact/{uuid}")
        );
        assert_eq!(full_web_fetch(&full), full, "a subagent's tools are worded again");
        let plain = full_web_fetch("- Responses are cached for 15 minutes per URL.");
        assert!(!plain.contains("artifact links") && !plain.contains('{'));
    }
}
