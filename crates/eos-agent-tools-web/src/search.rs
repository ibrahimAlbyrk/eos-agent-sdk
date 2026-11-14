//! WebSearch: one side call that forces the API's server-side web search tool,
//! then hands the model the commentary and the links found.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eos_agent_core::{BoxFuture, ModelClient, Tool, ToolContext, ToolOutput, ToolSpec};
use eos_agent_types::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::side::{self, SideModel};

pub const WEB_SEARCH: &str = "WebSearch";
const DESCRIPTION: &str = include_str!("../prompts/WebSearch.md");
const SCHEMA: &str = include_str!("../WebSearch.schema.json");
const SYSTEM: &str = "You run a web search for another Claude model, which is waiting on the result and can search \
again. Search once, then reply with a brief summary of the results.";
const MAX_USES: u32 = 8;
/// Claude Code's `CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION` default.
pub const DEFAULT_SEARCH_BUDGET: u32 = 200;
const REMINDER: &str =
    "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.";

#[derive(Debug, Clone)]
pub struct WebSearchConfig {
    /// Claude Code searches with the main loop's model.
    pub side: SideModel,
    /// Searches per session when no shared [`SessionSearchBudget`] is installed.
    pub budget: u32,
    /// Sent with `defer_loading`, as Claude Code does.
    pub deferred: bool,
}

impl WebSearchConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self { side: SideModel::new(model), budget: DEFAULT_SEARCH_BUDGET, deferred: true }
    }
}

/// How many searches a session may still run. Every agent of a session
/// shares one, so a fan-out of agents cannot multiply the allowance.
#[derive(Debug)]
pub struct SearchBudget {
    cap: u32,
    used: AtomicU32,
}

impl SearchBudget {
    pub fn new(cap: u32) -> Self {
        Self { cap, used: AtomicU32::new(0) }
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }

    fn try_consume(&self) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        while used < self.cap {
            match self.used.compare_exchange_weak(used, used + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
        false
    }
}

/// Put the same budget into every agent's extensions to share it across a session.
#[derive(Debug, Clone)]
pub struct SessionSearchBudget(pub Arc<SearchBudget>);

pub struct WebSearchTool {
    spec: ToolSpec,
    client: Arc<dyn ModelClient>,
    config: WebSearchConfig,
}

impl std::fmt::Debug for WebSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchTool").field("model", &self.config.side.model).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Input {
    query: String,
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ServerTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_domains: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_domains: Option<&'a [String]>,
    max_uses: u32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Link {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

/// One entry of a search's outcome, in the order the model produced it.
#[derive(Debug, PartialEq)]
pub(crate) enum Found {
    Commentary(String),
    Links(Vec<Link>),
}

impl WebSearchTool {
    pub fn new(client: Arc<dyn ModelClient>, config: WebSearchConfig) -> Self {
        let schema = serde_json::from_str(SCHEMA).expect("the bundled schema is JSON");
        let mut spec = ToolSpec::new(WEB_SEARCH, DESCRIPTION, schema);
        if config.deferred {
            spec = spec.deferred().with_search_hint("search the web for current information");
        }
        Self { spec, client, config }
    }

    async fn run(&self, input: &RawValue, ctx: ToolContext<'_>) -> ToolOutput {
        let input: Input = match serde_json::from_str(input.get()) {
            Ok(input) => input,
            Err(e) => return invalid(&format!("InputValidationError: {e}")),
        };
        if input.query.trim().is_empty() {
            return invalid("Error: Missing query");
        }
        if input.query.chars().count() < 2 {
            return invalid("InputValidationError: query must contain at least 2 characters");
        }
        let has = |d: &Option<Vec<String>>| d.as_ref().is_some_and(|d| !d.is_empty());
        if has(&input.allowed_domains) && has(&input.blocked_domains) {
            return invalid("Error: Cannot specify both allowed_domains and blocked_domains in the same request");
        }

        let budget = match ctx.extensions.get::<SessionSearchBudget>() {
            Some(shared) => shared,
            None => ctx
                .extensions
                .get_or_insert_with(|| SessionSearchBudget(Arc::new(SearchBudget::new(self.config.budget)))),
        };
        if !budget.0.try_consume() {
            return ToolOutput::text(format_results(&input.query, &[Found::Commentary(budget_spent(budget.0.cap()))]));
        }

        let server_tool = ServerTool {
            kind: "web_search_20250305",
            name: "web_search",
            allowed_domains: input.allowed_domains.as_deref(),
            blocked_domains: input.blocked_domains.as_deref(),
            max_uses: MAX_USES,
        };
        let mut request =
            self.config.side.request(Some(SYSTEM), &format!("Perform a web search for the query: {}", input.query));
        request.auth = side::agent_auth(ctx.extensions);
        request.tools = Some(side::json_bytes(&serde_json::json!([server_tool])));
        // A model that always thinks takes no forced call: the prompt asks for the search.
        let choice = if self.config.side.always_thinks {
            serde_json::json!({"type": "auto"})
        } else {
            serde_json::json!({"type": "tool", "name": "web_search"})
        };
        request.extra = Some(side::json_bytes(&serde_json::json!({ "tool_choice": choice })));

        match side::complete(&*self.client, &request, &self.config.side.retry).await {
            Ok(content) => ToolOutput::text(format_results(&input.query, &collect(&content))),
            Err(e) => ToolOutput::error(format!("Web search failed: {e}")),
        }
    }
}

impl Tool for WebSearchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(self.run(input, ctx))
    }
}

fn invalid(message: &str) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

fn budget_spent(cap: u32) -> String {
    let calls = if cap == 1 { "call" } else { "calls" };
    format!(
        "Web search was not performed: this session's web search budget is used up (limit: {cap} {WEB_SEARCH} {calls}, \
shared by every agent in this session; it does not refill). Continue with the information already gathered instead \
of searching again, and say so in your answer; do not work around the limit by querying search engines with curl or \
wget from Bash, by querying them in a browser or through a computer-use tool, or by searching or fetching through a \
third-party reader, proxy or archive service."
    )
}

/// Commentary between searches and each search's links, in stream order.
pub(crate) fn collect(content: &[ContentBlock]) -> Vec<Found> {
    #[derive(Deserialize)]
    struct ResultBlock {
        content: serde_json::Value,
    }
    let mut found = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, found: &mut Vec<Found>| {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            found.push(Found::Commentary(trimmed.to_owned()));
        }
        text.clear();
    };
    for block in content {
        match block {
            ContentBlock::ServerToolUse { .. } => flush(&mut text, &mut found),
            ContentBlock::Text { text: more, .. } => text.push_str(more),
            ContentBlock::Unknown(raw) if block.kind() == Some("web_search_tool_result") => {
                let Ok(result) = serde_json::from_str::<ResultBlock>(raw.get()) else { continue };
                match result.content {
                    serde_json::Value::Array(hits) => found.push(Found::Links(
                        hits.into_iter()
                            .map(|hit| Link {
                                title: hit.get("title").and_then(|t| t.as_str()).map(str::to_owned),
                                url: hit.get("url").and_then(|u| u.as_str()).map(str::to_owned),
                            })
                            .collect(),
                    )),
                    error => {
                        let code = error.get("error_code").and_then(|c| c.as_str()).unwrap_or("unknown");
                        found.push(Found::Commentary(format!("Web search error: {code}")));
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut text, &mut found);
    found
}

/// What the main model reads, exactly as Claude Code lays it out.
pub(crate) fn format_results(query: &str, found: &[Found]) -> String {
    let mut out = format!("Web search results for query: \"{query}\"\n\n");
    for entry in found {
        match entry {
            Found::Commentary(text) => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            Found::Links(links) if !links.is_empty() => {
                out.push_str("Links: ");
                out.push_str(&serde_json::to_string(links).expect("links serialize"));
                out.push_str("\n\n");
            }
            Found::Links(_) => out.push_str("No links found.\n\n"),
        }
    }
    out.push('\n');
    out.push_str(REMINDER);
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_shared_and_capped() {
        let budget = SearchBudget::new(2);
        assert!(budget.try_consume());
        assert!(budget.try_consume());
        assert!(!budget.try_consume());
    }

    #[test]
    fn formats_like_claude_code() {
        let found = vec![
            Found::Commentary("Intro".into()),
            Found::Links(vec![Link { title: Some("A".into()), url: Some("https://a.dev".into()) }]),
            Found::Links(vec![]),
        ];
        assert_eq!(
            format_results("rust", &found),
            "Web search results for query: \"rust\"\n\nIntro\n\nLinks: [{\"title\":\"A\",\"url\":\"https://a.dev\"}]\
\n\nNo links found.\n\n\nREMINDER: You MUST include the sources above in your response to the user using markdown \
hyperlinks."
        );
    }
}
