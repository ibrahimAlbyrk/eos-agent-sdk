//! WebFetch: fetch a page, turn it into markdown, and have a small model
//! answer the caller's prompt against it.

mod cache;
mod http;
mod notes;
mod page;
mod text;
mod urls;

use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{BoxFuture, ModelClient, RequestAuth, Tool, ToolContext, ToolOutput, ToolSpec, Workspace};
use eos_agent_types::ContentBlock;
use reqwest::Url;
use serde_json::value::RawValue;

pub use cache::{DEFAULT_CACHE_BYTES, DEFAULT_CACHE_TTL, FetchCache};

use crate::side::{self, SideModel};
use cache::Page;
use http::{FetchError, Limits, Outcome};
use notes::{FailureLog, FetchHistory};

pub const WEB_FETCH: &str = "WebFetch";
/// Claude Code's small fast model, which answers the prompt against the page.
pub const DEFAULT_FETCH_MODEL: &str = "claude-haiku-5-5";
const DESCRIPTION: &str = include_str!("../../prompts/WebFetch.md");
const ARTIFACT_LINKS: &str = include_str!("../../prompts/WebFetchArtifactLinks.md");
const SCHEMA: &str = include_str!("../../WebFetch.schema.json");
/// Page text beyond this many characters is not given to the model in one call.
const MAX_MODEL_INPUT: usize = 100_000;

#[derive(Debug, Clone)]
pub struct WebFetchConfig {
    pub side: SideModel,
    /// Shared by every agent (and every tool instance) that should see the same pages.
    pub cache: Arc<FetchCache>,
    pub user_agent: String,
    pub request_timeout: Duration,
    /// Over the whole fetch, redirects included.
    pub deadline: Duration,
    pub max_body: usize,
    /// `http:` is fetched as `https:`. Off only for tests against a local server.
    pub https_upgrade: bool,
    /// Preapproved hosts serving `text/markdown` under 100 000 characters get
    /// the page itself, no model step (Claude Code's rule).
    pub raw_markdown: bool,
    /// Short notes instead of refetching a URL that just failed, or a fetch
    /// the agent already holds the answer to.
    pub repeat_notes: bool,
    /// The session has the Artifact tool: the description tells the model to
    /// read claude.ai artifact links with it.
    pub artifact_tool: bool,
    /// Sent with `defer_loading`, as Claude Code does.
    pub deferred: bool,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            side: SideModel::new(DEFAULT_FETCH_MODEL),
            cache: Arc::new(FetchCache::default()),
            user_agent: concat!(
                "Claude-User (eos-agent/",
                env!("CARGO_PKG_VERSION"),
                "; +https://support.anthropic.com/)"
            )
            .into(),
            request_timeout: Duration::from_secs(60),
            deadline: Duration::from_secs(300),
            max_body: 10 * 1024 * 1024,
            https_upgrade: true,
            raw_markdown: true,
            repeat_notes: true,
            artifact_tool: false,
            deferred: true,
        }
    }
}

pub struct WebFetchTool {
    spec: ToolSpec,
    client: Arc<dyn ModelClient>,
    http: reqwest::Client,
    config: WebFetchConfig,
    failures: FailureLog,
}

impl std::fmt::Debug for WebFetchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebFetchTool").field("model", &self.config.side.model).finish_non_exhaustive()
    }
}

struct Input {
    url: String,
    prompt: String,
    offset: usize,
}

enum Loaded {
    Page(Arc<Page>),
    Redirect { from: Url, to: Url, status: u16 },
    HttpError { status: u16, retry_after: Option<String> },
}

impl WebFetchTool {
    pub fn new(client: Arc<dyn ModelClient>, config: WebFetchConfig) -> Self {
        let description = DESCRIPTION
            .replace("{artifact_links}", if config.artifact_tool { ARTIFACT_LINKS } else { "" })
            .replace("{cache_ttl}", &ttl_words(config.cache.ttl()));
        let schema = serde_json::from_str(SCHEMA).expect("the bundled schema is JSON");
        let mut spec = ToolSpec::new(WEB_FETCH, &description, schema);
        if config.deferred {
            spec = spec.deferred().with_search_hint("fetch and extract content from a URL");
        }
        Self { spec, client, http: http::client(), config, failures: FailureLog::default() }
    }

    async fn run(&self, input: &RawValue, ctx: ToolContext<'_>) -> ToolOutput {
        let input = match parse(input) {
            Ok(input) => input,
            Err(message) => return invalid(&message),
        };
        let Ok(parsed) = Url::parse(&input.url) else {
            return invalid(&format!("Error: Invalid URL \"{}\". The URL provided could not be parsed.", input.url));
        };
        if let Some(id) = urls::claude_share_id(&parsed) {
            return ToolOutput::text(text::share_link(id));
        }

        let key = notes::call_key(&input.prompt, input.offset);
        let history = ctx.extensions.get_or_default::<FetchHistory>();
        if self.config.repeat_notes {
            let cached = self.config.cache.peek(&input.url);
            if let Some(earlier) = history.repeat(&input.url, key, cached.as_ref()) {
                let ttl = ttl_words(self.config.cache.ttl());
                return ToolOutput::text(text::already_fetched(&input.url, input.offset, &earlier, &ttl));
            }
            if cached.is_none()
                && let Some((kind, ago)) = self.failures.tell(&self.target(&parsed), ctx.agent_id)
            {
                return ToolOutput::text(text::failed_earlier(&input.url, kind, ago));
            }
        }

        let scratch = ctx.extensions.get::<Workspace>().map(|w| w.scratch().to_path_buf());
        let page = match self.load(&input.url, scratch.as_deref()).await {
            Ok(Loaded::Page(page)) => page,
            Ok(Loaded::Redirect { from, to, status }) => {
                return ToolOutput::text(text::redirect(&from, &to, status, &input.prompt, input.offset));
            }
            Ok(Loaded::HttpError { status, retry_after }) => {
                return ToolOutput::text(text::http_error(status, retry_after.as_deref()));
            }
            Err(message) => return ToolOutput::error(message),
        };

        let result = match self.answer(&page, &parsed, &input, side::agent_auth(ctx.extensions)).await {
            Ok(result) => result,
            Err(message) => return ToolOutput::error(message),
        };
        if self.config.repeat_notes && !page.content.trim().is_empty() && !result.trim().is_empty() {
            history.remember(&input.url, key, ctx.tool_use_id, &page);
        }
        ToolOutput::text(result)
    }

    /// The URL actually requested (after the https upgrade).
    fn target(&self, url: &Url) -> String {
        let mut url = url.clone();
        if self.config.https_upgrade && url.scheme() == "http" {
            let _ = url.set_scheme("https");
        }
        url.to_string()
    }

    async fn load(&self, raw: &str, scratch: Option<&std::path::Path>) -> Result<Loaded, String> {
        if let Some(page) = self.config.cache.get(raw) {
            return Ok(Loaded::Page(page));
        }
        let url = urls::check(raw).map_err(|problem| match problem {
            urls::UrlProblem::Dotless => "WebFetch cannot fetch localhost or other hostnames without a dot. To reach \
a local server, use Bash with curl instead."
                .to_owned(),
            urls::UrlProblem::Invalid => "Invalid URL".to_owned(),
        })?;
        let target = self.target(&url);
        let url = Url::parse(&target).map_err(|_| "Invalid URL".to_owned())?;
        let limits = Limits {
            request_timeout: self.config.request_timeout,
            deadline: self.config.deadline,
            max_body: self.config.max_body,
        };
        match http::fetch(&self.http, url, &limits, &self.config.user_agent).await {
            Ok(Outcome::Body { content_type, body }) => {
                self.failures.clear(&target);
                let (page, size) = page::build(content_type, body, scratch).await;
                let page = Arc::new(page);
                self.config.cache.insert(raw, Arc::clone(&page), size);
                Ok(Loaded::Page(page))
            }
            Ok(Outcome::Redirect { from, to, status }) => Ok(Loaded::Redirect { from, to, status }),
            Ok(Outcome::HttpError { status, retry_after }) => {
                let kind = match status {
                    404 | 410 => "url_not_found",
                    400..=499 => "client_error",
                    _ => "server_error",
                };
                self.failures.record(&target, kind);
                Ok(Loaded::HttpError { status, retry_after })
            }
            Err(e) => {
                if matches!(e, FetchError::Transport(_)) {
                    self.failures.record(&target, "unreachable");
                }
                Err(e.to_string())
            }
        }
    }

    /// The tool result for a fetched page: the page itself (preapproved
    /// markdown), or the small model's answer with a note when it read only
    /// part of the page.
    async fn answer(&self, page: &Page, url: &Url, input: &Input, auth: Option<RequestAuth>) -> Result<String, String> {
        let content = &page.content;
        let total = content.chars().count();
        let start = content.char_indices().nth(input.offset).map_or(content.len(), |(i, _)| i);
        let rest = &content[start..];
        let rest_chars = total - input.offset.min(total);
        let preapproved = urls::is_preapproved(url);
        let binary = page::is_binary(&page.content_type);

        let mut out = if input.offset > 0 && rest.is_empty() {
            format!("Nothing left to read from offset {}: this page's text is {total} characters long.", input.offset)
        } else if self.config.raw_markdown
            && preapproved
            && page.content_type.contains("text/markdown")
            && rest_chars < MAX_MODEL_INPUT
        {
            rest.to_owned()
        } else {
            let lead = if input.offset > 0 {
                format!(
                    "The content below is one part of a longer page: it starts {} characters into the page's {total}.\n",
                    input.offset
                )
            } else {
                String::new()
            };
            let cut = rest.char_indices().nth(MAX_MODEL_INPUT).map_or(rest.len(), |(i, _)| i);
            let mut answer = self.ask(&lead, &rest[..cut], cut < rest.len(), &input.prompt, preapproved, auth).await?;
            let from = total - rest_chars;
            let to = from + rest_chars.min(MAX_MODEL_INPUT);
            if to < total && !binary {
                answer.push_str(&text::partial_note(total, from, to));
            }
            answer
        };
        if let Some((path, size)) = &page.saved {
            out.push_str(&format!(
                "\n\n[Binary content ({}, {}) also saved to {}]",
                page::short_mime(&page.content_type),
                page::format_size(*size),
                path.display()
            ));
        }
        Ok(out)
    }

    async fn ask(
        &self,
        lead: &str,
        content: &str,
        cut: bool,
        prompt: &str,
        preapproved: bool,
        auth: Option<RequestAuth>,
    ) -> Result<String, String> {
        let mut request = self.config.side.request(None, &text::apply_prompt(lead, content, cut, prompt, preapproved));
        request.auth = auth;
        let blocks =
            side::complete(&*self.client, &request, &self.config.side.retry).await.map_err(|e| e.to_string())?;
        // A model that thinks answers `[thinking, text…]`: the answer is its text blocks, in order.
        let answer: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        Ok(if answer.is_empty() { "No response from model".to_owned() } else { answer })
    }
}

impl Tool for WebFetchTool {
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

/// `15 minutes`, as the description and notes say it.
fn ttl_words(ttl: Duration) -> String {
    let minutes = (ttl.as_secs_f64() / 60.0).round().max(1.0) as u64;
    format!("{minutes} {}", if minutes == 1 { "minute" } else { "minutes" })
}

fn invalid(message: &str) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

/// Like Claude Code, `offset` may come as a number or a numeric string.
fn parse(input: &RawValue) -> Result<Input, String> {
    let value: serde_json::Value =
        serde_json::from_str(input.get()).map_err(|e| format!("InputValidationError: {e}"))?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    let url = field("url").ok_or("InputValidationError: The required parameter `url` is missing")?;
    let prompt = field("prompt").ok_or("InputValidationError: The required parameter `prompt` is missing")?;
    let offset = match value.get("offset") {
        None | Some(serde_json::Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("InputValidationError: `offset` must be a non-negative integer")?,
    };
    Ok(Input { url, prompt, offset })
}
