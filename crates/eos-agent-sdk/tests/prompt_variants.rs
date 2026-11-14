//! Claude Code's per-model wordings, byte for byte as Claude Code 2.1.319
//! builds them: the full system prompt (models without `lean_prompt`), its
//! harness line (`kPe`) and the tool texts that depend on the model (`b0`).

mod common;

use std::sync::Arc;

use common::{Dirs, agent, mock};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::{Recorded, Reply};
use eos_agent_sdk::{FrameConfig, SandboxConfig, Sdk, WatchConfig};
use eos_agent_tools_shell::sandbox::Seatbelt;

const TURNS_LINE: &str = "\n - The system may send updates, reminders, or modifications to rules via mid-conversation \
system turns. These are system-controlled, unlike function results.\n";
const TAGS_LINE: &str = "\n - Tool results and user messages may include <system-reminder> or other tags. Tags contain \
information from the system. They bear no direct relation to the specific tool results or user messages in which \
they appear.\n";

/// The first request an agent on `model` sends.
async fn first_request(name: &str, model: &str) -> Recorded {
    let dirs = Dirs::new(name);
    let mock = mock([Reply::text("ok")]).await;
    let sdk = common::sdk_with_model(&mock, &dirs, model).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("hello").await.expect("alive").expect("turn");
    mock.requests().remove(0)
}

/// The request after ToolSearch loaded the web tools (deferred tools are
/// declared once loaded).
async fn web_tools_request(name: &str, model: &str) -> Recorded {
    let dirs = Dirs::new(name);
    let mock = mock([Reply::tool_use("toolu_s", "ToolSearch", SELECT_WEB), Reply::text("ok")]).await;
    let sdk = common::sdk_with_model(&mock, &dirs, model).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("hello").await.expect("alive").expect("turn");
    mock.requests().remove(1)
}

const SELECT_WEB: &str = r#"{"query":"select:WebFetch,WebSearch","max_results":5}"#;

/// The description `request` sends for the tool `name`.
fn description(request: &Recorded, name: &str) -> String {
    let body = request.json();
    let tool = body["tools"].as_array().and_then(|t| t.iter().find(|t| t["name"] == name).cloned());
    tool.and_then(|t| t["description"].as_str().map(str::to_owned)).unwrap_or_else(|| panic!("no {name}"))
}

fn roles(request: &Recorded) -> Vec<String> {
    let body = request.json();
    body["messages"].as_array().into_iter().flatten().map(|m| m["role"].as_str().unwrap_or("").to_owned()).collect()
}

/// Mythos 5 has no `lean_prompt` but takes system turns, and Claude Code's
/// `kPe` holds for it: the full prompt tells it about system turns.
#[tokio::test]
async fn mythos_5_gets_the_full_prompt_naming_system_turns() {
    let request = first_request("variants-mythos", "claude-mythos-5").await;
    let system = common::system_text(&request);
    assert!(system.contains("# Doing tasks"), "the full prompt");
    assert!(system.contains(TURNS_LINE) && !system.contains(TAGS_LINE), "{system}");
    assert_eq!(roles(&request), ["user", "system"], "the session note goes out as a system turn");
}

/// Sonnet 5 takes system turns too, but `kPe` leaves it out; models
/// before Opus 4.8 take none.
#[tokio::test]
async fn other_full_prompt_models_keep_the_tags_line() {
    for (name, model, turns) in [
        ("variants-sonnet-5", "claude-sonnet-5", ["user", "system"].as_slice()),
        ("variants-haiku-4-5", "claude-haiku-4-5", ["user"].as_slice()),
    ] {
        let request = first_request(name, model).await;
        let system = common::system_text(&request);
        assert!(system.contains("# Doing tasks"), "{model}: the full prompt");
        assert!(system.contains(TAGS_LINE) && !system.contains(TURNS_LINE), "{model}");
        assert_eq!(roles(&request), turns, "{model}");
    }
}

const GLOB_LEAN: &str = "Fast file pattern matching. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". \
Returns matching file paths sorted by modification time.";
const GLOB_FULL: &str = r#"- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like "**/*.js" or "src/**/*.ts"
- Returns matching file paths sorted by modification time
- Use this tool when you need to find files by name patterns
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead (if available)"#;
const GREP_LEAN: &str = r#"Content search built on ripgrep. Prefer this over `grep`/`rg` via Bash — results integrate with the permission UI and file links.

- Full regex syntax (e.g. "log.*Error", "function\s+\w+"). Ripgrep, not grep — escape literal braces (`interface\{\}`).
- Filter with `glob` (e.g. "**/*.tsx") or `type` (e.g. "js", "py", "rust").
- `output_mode`: "content" (matching lines), "files_with_matches" (paths only, default), or "count".
- `multiline: true` for patterns that span lines."#;
const GREP_FULL: &str = r#"A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. The Grep tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., "log.*Error", "function\s+\w+")
  - Filter files with glob parameter (e.g., "*.js", "**/*.tsx") or type parameter (e.g., "js", "py", "rust")
  - Output modes: "content" shows matching lines, "files_with_matches" shows only file paths (default), "count" shows match counts
  - Use Agent tool (if available) for open-ended searches requiring multiple rounds
  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\{\}` to find `interface{}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \{[\s\S]*?field`, use `multiline: true`
"#;

/// Glob and Grep (`oks`, `iks`): Claude Code's lean texts on lean models,
/// its full ones where the full prompt goes.
#[tokio::test]
async fn search_tools_are_worded_for_the_model() {
    let lean = first_request("variants-search-lean", "claude-opus-5-5").await;
    assert_eq!(description(&lean, "Glob"), GLOB_LEAN);
    assert_eq!(description(&lean, "Grep"), GREP_LEAN);
    for (name, model) in [("variants-search-haiku", "claude-haiku-4-5"), ("variants-search-mythos", "claude-mythos-5")]
    {
        let full = first_request(name, model).await;
        assert_eq!(description(&full, "Glob"), GLOB_FULL, "{model}");
        assert_eq!(description(&full, "Grep"), GREP_FULL, "{model}");
    }
}

const FETCH_LEAN: &str = r#"Fetches a URL, converts the page to markdown, and answers `prompt` against it using a small fast model.

- Fails on authenticated/private URLs — use an authenticated MCP tool or `gh` for those instead.
- Fails on localhost and other hostnames without a dot; for a local server, use curl via Bash.
- HTTP is upgraded to HTTPS. Cross-host redirects are returned to you rather than followed; call again with the redirect URL.
- Responses are cached for 15 minutes per URL."#;
const FETCH_LEAN_ARTIFACTS: &str = r#"Fetches a URL, converts the page to markdown, and answers `prompt` against it using a small fast model.

- Fails on authenticated/private URLs — use an authenticated MCP tool or `gh` for those instead. claude.ai artifact links (claude.ai/artifact/{id} or claude.ai/code/artifact/{uuid}) are published artifacts: read them with the Artifact tool (action "read"), not WebFetch or curl.
- Fails on localhost and other hostnames without a dot; for a local server, use curl via Bash.
- HTTP is upgraded to HTTPS. Cross-host redirects are returned to you rather than followed; call again with the redirect URL.
- Responses are cached for 15 minutes per URL."#;
const FETCH_FULL: &str = r#"IMPORTANT: WebFetch WILL FAIL for authenticated or private URLs. Before using this tool, check if the URL points to an authenticated service (e.g. Google Docs, Confluence, Jira, GitHub). If so, look for a specialized MCP tool that provides authenticated access.

- Fetches content from a specified URL and processes it using an AI model
- Takes a URL and a prompt as input
- Fetches the URL content, converts HTML to markdown
- Processes the content with the prompt using a small, fast model
- Returns the model's response about the content
- Use this tool when you need to retrieve and analyze web content

Usage notes:
  - IMPORTANT: If an MCP-provided web fetch tool is available, prefer using that tool instead of this one, as it may have fewer restrictions.
  - The URL must be a fully-formed valid URL
  - HTTP URLs will be automatically upgraded to HTTPS
  - localhost and other hostnames without a dot are not supported; for a local server, use curl via Bash
  - The prompt should describe what information you want to extract from the page
  - This tool is read-only and does not modify any files
  - Results may be summarized if the content is very large
  - Includes a self-cleaning cache (entries expire after 15 minutes) for faster responses when repeatedly accessing the same URL
  - When a URL redirects to a different host, the tool will inform you and provide the redirect URL in a special format. You should then make a new WebFetch request with the redirect URL to fetch the content.
  - For GitHub URLs, prefer using the gh CLI via Bash instead (e.g., gh pr view, gh issue view, gh api).
"#;
const FETCH_FULL_ARTIFACTS: &str = r#"IMPORTANT: WebFetch WILL FAIL for authenticated or private URLs. Before using this tool, check if the URL points to an authenticated service (e.g. Google Docs, Confluence, Jira, GitHub). If so, look for a specialized MCP tool that provides authenticated access.
- claude.ai artifact links (claude.ai/artifact/{id} or claude.ai/code/artifact/{uuid}, including preview.claude.ai) are published artifacts: read them with the Artifact tool (action "read"), not WebFetch, curl or a headless browser.

- Fetches content from a specified URL and processes it using an AI model
- Takes a URL and a prompt as input
- Fetches the URL content, converts HTML to markdown
- Processes the content with the prompt using a small, fast model
- Returns the model's response about the content
- Use this tool when you need to retrieve and analyze web content

Usage notes:
  - IMPORTANT: If an MCP-provided web fetch tool is available, prefer using that tool instead of this one, as it may have fewer restrictions.
  - The URL must be a fully-formed valid URL
  - HTTP URLs will be automatically upgraded to HTTPS
  - localhost and other hostnames without a dot are not supported; for a local server, use curl via Bash
  - The prompt should describe what information you want to extract from the page
  - This tool is read-only and does not modify any files
  - Results may be summarized if the content is very large
  - Includes a self-cleaning cache (entries expire after 15 minutes) for faster responses when repeatedly accessing the same URL
  - When a URL redirects to a different host, the tool will inform you and provide the redirect URL in a special format. You should then make a new WebFetch request with the redirect URL to fetch the content.
  - For GitHub URLs, prefer using the gh CLI via Bash instead (e.g., gh pr view, gh issue view, gh api).
"#;
const SEARCH_LEAN: &str = r#"Search the web. Returns result blocks with titles and URLs. US-only.

- The current month is (provided in the conversation below) — use this when searching for recent information.
- `allowed_domains` / `blocked_domains` filter results.
- After answering from results, end with a "Sources:" list of the URLs you used as markdown links."#;
const SEARCH_FULL: &str = r#"
- Allows Claude to search the web and use the results to inform responses
- Provides up-to-date information for current events and recent data
- Returns search result information formatted as search result blocks, including links as markdown hyperlinks
- Use this tool for accessing information beyond Claude's knowledge cutoff
- Searches are performed automatically within a single API call

CRITICAL REQUIREMENT - You MUST follow this:
  - After answering the user's question, you MUST include a "Sources:" section at the end of your response
  - In the Sources section, list all relevant URLs from the search results as markdown hyperlinks: [Title](URL)
  - This is MANDATORY - never skip including sources in your response
  - Example format:

    [Your answer here]

    Sources:
    - [Source Title 1](https://example.com/1)
    - [Source Title 2](https://example.com/2)

Usage notes:
  - Domain filtering is supported to include or block specific websites
  - Web search is only available in the US

IMPORTANT - Use the correct year in search queries:
  - The current month is (provided in the conversation below). You MUST use this year when searching for recent information, documentation, or current events.
  - Example: If the user asks for "latest React docs", search for "React documentation" with the current year, NOT last year
"#;

/// Whether `request` sends `name` behind ToolSearch.
fn deferred(request: &Recorded, name: &str) -> bool {
    let body = request.json();
    body["tools"].as_array().into_iter().flatten().any(|t| t["name"] == name && t["defer_loading"] == true)
}

/// WebFetch and WebSearch (`aBi`, `oWi`), sent deferred: Claude Code's lean
/// texts on lean models, its full ones where the full prompt goes.
#[tokio::test]
async fn web_tools_are_worded_for_the_model() {
    let lean = web_tools_request("variants-web-lean", "claude-opus-5-5").await;
    let full = web_tools_request("variants-web-full", "claude-haiku-4-5").await;
    for request in [&lean, &full] {
        assert!(deferred(request, "WebFetch") && deferred(request, "WebSearch"));
    }
    assert_eq!(description(&lean, "WebFetch"), FETCH_LEAN);
    assert_eq!(description(&lean, "WebSearch"), SEARCH_LEAN);
    assert_eq!(description(&full, "WebFetch"), FETCH_FULL);
    assert_eq!(description(&full, "WebSearch"), SEARCH_FULL);
}

/// With the Artifact tool, both WebFetch texts send artifact links to it
/// (`f` in the lean text, `p` in the full one).
#[tokio::test]
async fn web_fetch_points_artifact_links_at_the_artifact_tool() {
    for (name, model, expected) in [
        ("variants-fetch-artifacts-lean", "claude-opus-5-5", FETCH_LEAN_ARTIFACTS),
        ("variants-fetch-artifacts-full", "claude-mythos-5", FETCH_FULL_ARTIFACTS),
    ] {
        let dirs = Dirs::new(name);
        let mock = mock([Reply::tool_use("toolu_s", "ToolSearch", SELECT_WEB), Reply::text("ok")]).await;
        let sdk = Sdk::builder()
            .auth(Arc::new(StaticCredential(Credential::Bearer("token".into()))))
            .base_url(mock.base_url())
            .model(model)
            .claude_paths(ClaudePaths::for_home(&dirs.home()))
            .data_dir(dirs.root.join("data"))
            .artifacts(true)
            .artifact_api(FrameConfig { base_url: mock.base_url(), ..FrameConfig::default() }, WatchConfig::default())
            .build()
            .await
            .expect("sdk");
        let (agent, _events) = agent(&sdk, &dirs).await;
        agent.send_text("hello").await.expect("alive").expect("turn");
        let request = mock.requests().remove(1);
        assert!(common::tool_names(&request).iter().any(|n| n == "Artifact"), "{model}");
        assert_eq!(description(&request, "WebFetch"), expected, "{model}");
    }
}

const ASK_LEAN: &str = r#"Use this tool only when you are blocked on a decision that is genuinely the user's to make: one you cannot resolve from the request, the code, or sensible defaults.

Usage notes:
- Users will always be able to select "Other" to provide custom text input
- Use multiSelect: true to allow multiple answers to be selected for a question
- If you recommend a specific option, make that the first option in the list and add "(Recommended)" at the end of the label

Reserve this for decisions where the user's answer changes what you do next — not for choices with a conventional default or facts you can verify in the codebase yourself. In those cases pick the obvious option, mention it in your response, and proceed.

The user may see only the question dialog: tool results and text you wrote earlier in this turn may not be on their screen while it is open. Make each question stand on its own: if it depends on something you found, say so briefly, and name the items you are asking about in the question itself.
"#;
const ASK_FULL: &str = r#"Use this tool only when you are blocked on a decision that is genuinely the user's to make: one you cannot resolve from the request, the code, or sensible defaults.

Usage notes:
- Users will always be able to select "Other" to provide custom text input
- Use multiSelect: true to allow multiple answers to be selected for a question
- If you recommend a specific option, make that the first option in the list and add "(Recommended)" at the end of the label

The user may see only the question dialog: tool results and text you wrote earlier in this turn may not be on their screen while it is open. Make each question stand on its own: if it depends on something you found, say so briefly, and name the items you are asking about in the question itself.
"#;

/// AskUserQuestion (`cs`): the lean text adds the paragraph on when to
/// reserve a question (`Y$i`); the full one goes without it.
#[tokio::test]
async fn ask_user_question_is_worded_for_the_model() {
    let lean = first_request("variants-ask-lean", "claude-opus-5-5").await;
    assert_eq!(description(&lean, "AskUserQuestion"), ASK_LEAN);
    let full = first_request("variants-ask-full", "claude-sonnet-4-6").await;
    assert_eq!(description(&full, "AskUserQuestion"), ASK_FULL);
}

/// Claude Code's full Bash text (`Krn`) has no sandbox wording: with the
/// sandbox on it is the same text, over the sandbox's schema.
#[tokio::test]
async fn full_bash_text_is_the_same_with_the_sandbox_on() {
    if !Seatbelt::available() {
        eprintln!("skipped: sandbox-exec is not available here");
        return;
    }
    let off = first_request("variants-bash-off", "claude-haiku-4-5").await;
    let dirs = Dirs::new("variants-bash-sandbox");
    let mock = mock([Reply::text("ok")]).await;
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("claude-haiku-4-5")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .sandbox(serde_json::from_value::<SandboxConfig>(serde_json::json!({"enabled": true})).expect("config"))
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("hello").await.expect("alive").expect("turn");
    let on = mock.requests().remove(0);
    let text = description(&on, "Bash");
    assert!(text.starts_with("Executes a given bash command and returns its output.\n"), "the full text");
    assert_eq!(text, description(&off, "Bash"));
    let body = on.json();
    let bash = body["tools"].as_array().and_then(|t| t.iter().find(|t| t["name"] == "Bash").cloned()).expect("Bash");
    assert!(bash["input_schema"]["properties"].get("allowed_domains").is_some(), "the sandbox's schema");
}
