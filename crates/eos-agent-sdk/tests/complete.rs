//! One-shot requests outside any conversation, and the command listing for
//! a host's menu.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Dirs, builder, mock, sdk, sdk_with_model, system_text};
use eos_agent_core::{CancellationToken, TokenSource};
use eos_agent_engine::JsonlStore;
use eos_agent_mock_api::Reply;
use eos_agent_sdk::{Account, AgentOptions, CommandInfo, CompleteError, CompleteRequest, Provider, SkillOrigin};
use serde_json::json;

#[tokio::test]
async fn a_one_shot_request_has_no_tools_and_leaves_no_trace() {
    let dirs = Dirs::new("complete-plain");
    let mock = mock([Reply::overloaded(), Reply::text("Fix the login redirect")]).await;
    let transcripts = dirs.root.join("transcripts");
    let sdk = builder(&mock, &dirs)
        .model("claude-opus-5-5")
        .store(Arc::new(JsonlStore::new(&transcripts)))
        .build()
        .await
        .expect("sdk");
    let request = CompleteRequest::new("Name this chat: the login page redirects twice")
        .model("haiku")
        .system("You write short chat titles.")
        .effort("low")
        .max_tokens(200);
    let done = sdk.complete(request).await.expect("answered");

    assert_eq!(done.text, "Fix the login redirect");
    assert!(done.structured.is_none());
    assert_eq!(done.model, "claude-haiku-5-5");
    assert_eq!((done.usage.input_tokens, done.usage.output_tokens), (Some(10), Some(5)));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "an overloaded API is tried again");
    let body = requests[1].json();
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 200);
    assert!(body.get("tools").is_none(), "no tools");
    assert!(body.get("thinking").is_none(), "Haiku 5.5 always thinks: no thinking setting");
    assert_eq!(body["output_config"]["effort"], "low");
    let system = body["system"].as_array().expect("system");
    assert_eq!(system.len(), 2, "Claude Code's identity line, then the host's prompt");
    assert_eq!(system[1]["text"], "You write short chat titles.");
    assert!(system_text(&requests[1]).starts_with("You are a Claude agent"), "{}", system_text(&requests[1]));
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"][0]["text"], "Name this chat: the login page redirects twice");
    assert_eq!(sdk.agent_count(), 0, "no agent was started");
    assert!(!transcripts.exists(), "nothing was recorded");
}

#[tokio::test]
async fn a_schema_gets_a_checked_json_answer() {
    let dirs = Dirs::new("complete-schema");
    let mock = mock([
        Reply::tool_use("toolu_1", "StructuredOutput", r#"{"verdict":"keep","score":4}"#),
        Reply::tool_use("toolu_2", "StructuredOutput", r#"{"score":"high"}"#),
        Reply::text("I would keep it."),
    ])
    .await;
    // A model that takes thinking off takes a forced call too.
    let sdk = sdk_with_model(&mock, &dirs, "claude-sonnet-5").await;
    let schema = json!({
        "type": "object",
        "properties": {"verdict": {"type": "string"}, "score": {"type": "integer"}},
        "required": ["verdict"]
    });
    let ask = || CompleteRequest::new("Judge this memory.").schema(schema.clone());

    let done = sdk.complete(ask()).await.expect("answered");
    assert_eq!(done.structured.expect("structured").get(), r#"{"verdict":"keep","score":4}"#);
    assert_eq!(done.text, "");
    let body = mock.requests()[0].json();
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "StructuredOutput");
    assert_eq!(tools[0]["input_schema"], schema);
    assert_eq!(body["tool_choice"], json!({"type": "tool", "name": "StructuredOutput"}));

    let wrong = sdk.complete(ask()).await.expect_err("does not match");
    assert!(matches!(&wrong, CompleteError::Schema(why) if why.contains("verdict")), "{wrong:?}");
    let prose = sdk.complete(ask()).await.expect_err("no structured answer");
    assert!(matches!(prose, CompleteError::Schema(_)), "{prose:?}");
}

/// What the Messages API refuses in a request's `thinking`, effort and
/// `tool_choice`, model by model, from Anthropic's thinking docs (the table
/// under "Configuring thinking" and "Response prefill and forced tool use",
/// read 2026-11-13). The mock API takes anything; this is the check.
fn refusal(body: &serde_json::Value) -> Option<String> {
    const ALWAYS_THINK: &[&str] = &[
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5",
        "claude-fable-5-1",
        "claude-fable-5-2",
        "claude-mythos-5",
        "claude-mythos-5-1",
        "claude-mythos-5-2",
    ];
    const OFF_UP_TO_HIGH: &[&str] = &["claude-opus-5", "claude-haiku-5-5"];
    const NO_FORCED_TOOL: &[&str] = &[
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "claude-fable-5-2",
        "claude-mythos-5-1",
        "claude-mythos-5-2",
    ];
    const BUDGET_ONLY: &[&str] = &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-4-5"];
    let model = body["model"].as_str().unwrap_or_default();
    let thinking = body["thinking"]["type"].as_str();
    let effort = body["output_config"]["effort"].as_str();
    let above_high = matches!(effort, Some("xhigh" | "max"));
    let refused = match thinking {
        Some("disabled") if ALWAYS_THINK.contains(&model) => Some("thinking.type.disabled is not supported"),
        Some("disabled") if above_high && OFF_UP_TO_HIGH.contains(&model) => Some("disabled only at high or below"),
        Some("between_tools") if model != "claude-sonnet-5-5" || above_high => Some("between_tools"),
        Some("adaptive") if BUDGET_ONLY.contains(&model) => Some("adaptive thinking is not supported"),
        _ => None,
    };
    let forced = matches!(body["tool_choice"]["type"].as_str(), Some("tool" | "any"));
    let refused = refused.or_else(|| match thinking {
        _ if forced && NO_FORCED_TOOL.contains(&model) => {
            Some("tool_choice: type \"tool\" and \"any\" are not supported")
        }
        Some("enabled") if forced => Some("forced tool use with manual extended thinking"),
        _ => None,
    });
    refused.map(|why| format!("{model} (thinking {thinking:?}, effort {effort:?}): {why}"))
}

/// Auto-name, compaction, dreams and the loop judge go through here on
/// whatever model the host picks: each request must be one its model takes.
#[tokio::test]
async fn one_shot_requests_follow_each_models_thinking_rules() {
    const MODELS: &[&str] = &[
        "claude-haiku-5-5",
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-fable-5-1",
        "claude-fable-5-2",
        "claude-mythos-5-1",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-haiku-4-5",
    ];
    const EFFORTS: &[Option<&str>] = &[None, Some("max")];
    let dirs = Dirs::new("complete-models");
    let replies = MODELS.iter().flat_map(|_| EFFORTS).flat_map(|_| {
        [Reply::text("Fix the login"), Reply::tool_use("toolu_1", "StructuredOutput", r#"{"title":"Fix the login"}"#)]
    });
    let mock = mock(replies).await;
    let sdk = sdk(&mock, &dirs).await;
    let schema = json!({"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]});
    for model in MODELS {
        for effort in EFFORTS {
            for schema in [None, Some(&schema)] {
                let mut ask = CompleteRequest::new("Name this chat.").model(*model);
                ask.effort = effort.map(str::to_owned);
                ask.schema = schema.cloned();
                sdk.complete(ask).await.unwrap_or_else(|e| panic!("{model}: {e}"));
            }
        }
    }

    let bodies: Vec<serde_json::Value> = mock.requests().iter().map(|r| r.json()).collect();
    assert_eq!(bodies.len(), MODELS.len() * EFFORTS.len() * 2, "every answer came the first time");
    let refused: Vec<String> = bodies.iter().filter_map(refusal).collect();
    assert!(refused.is_empty(), "{refused:#?}");
    let off: std::collections::BTreeSet<&str> =
        bodies.iter().filter(|b| b["thinking"]["type"] == "disabled").filter_map(|b| b["model"].as_str()).collect();
    assert_eq!(
        off.into_iter().collect::<Vec<_>>(),
        ["claude-haiku-4-5", "claude-opus-4-8", "claude-opus-5", "claude-sonnet-5"],
        "thinking is off wherever the model takes that"
    );
    let opus_5_max = bodies.iter().find(|b| b["model"] == "claude-opus-5" && b.get("output_config").is_some());
    assert_eq!(opus_5_max.map(|b| &b["output_config"]["effort"]), Some(&json!("high")), "lowered to keep it off");
}

#[tokio::test]
async fn a_model_that_always_thinks_is_asked_again_for_its_structured_answer() {
    let dirs = Dirs::new("complete-nudge");
    let mock = mock([
        Reply::text("I would keep it."),
        Reply::tool_use("toolu_1", "StructuredOutput", r#"{"verdict":"keep"}"#),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let schema = json!({"type": "object", "properties": {"verdict": {"type": "string"}}, "required": ["verdict"]});
    let ask = CompleteRequest::new("Judge this memory.").model("opus").schema(schema);

    let done = sdk.complete(ask).await.expect("answered the second time");
    assert_eq!(done.structured.expect("structured").get(), r#"{"verdict":"keep"}"#);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0].json();
    assert_eq!(first["tool_choice"], json!({"type": "auto"}));
    assert!(first.get("thinking").is_none());
    let second = requests[1].json();
    let messages = second["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert_eq!(messages[1]["content"][0]["text"], "I would keep it.");
    assert!(messages[2]["content"][0]["text"].as_str().is_some_and(|t| t.contains("MUST call the StructuredOutput")));
}

/// WebSearch runs on the agent's model: one that always thinks gets no
/// thinking setting and a search it is asked for, not forced into.
#[tokio::test]
async fn web_search_on_a_model_that_always_thinks() {
    let dirs = Dirs::new("complete-web-search");
    let mock = mock([
        Reply::tool_use("toolu_1", "WebSearch", r#"{"query":"rust language"}"#),
        Reply::text("Rust is a language."),
        Reply::text("Found it."),
    ])
    .await;
    let sdk = builder(&mock, &dirs).model("claude-opus-5-5").build().await.expect("sdk");
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).tool_search(false)).await.expect("spawn");
    agent.send_input("Look up rust.").await.expect("input").expect("turn");

    let side = mock.requests()[1].json();
    assert_eq!(side["model"], "claude-opus-5-5");
    assert_eq!(side["tools"][0]["name"], "web_search");
    assert_eq!(side["tool_choice"], json!({"type": "auto"}));
    assert!(side.get("thinking").is_none());
    assert_eq!(refusal(&side), None);
}

#[tokio::test]
async fn a_one_shot_request_can_be_cancelled_and_needs_a_credential() {
    let dirs = Dirs::new("complete-cancel");
    let mock = mock([Reply::text("too late").slowed(Duration::from_secs(20))]).await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
    });
    let started = Instant::now();
    let cancelled = sdk.complete(CompleteRequest::new("Summarize.").cancel(cancel)).await.expect_err("cancelled");
    assert!(matches!(cancelled, CompleteError::Cancelled), "{cancelled:?}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let other = sdk.complete(CompleteRequest::new("Summarize.").model("gpt-5")).await.expect_err("no ChatGPT sign-in");
    assert!(matches!(other, CompleteError::NoProvider { provider: Provider::OpenAi, .. }), "{other:?}");
}

#[tokio::test]
async fn a_one_shot_request_runs_on_the_credential_it_names() {
    let dirs = Dirs::new("complete-auth");
    let mock = mock([Reply::text("own"), Reply::text("named")]).await;
    let sdk = sdk(&mock, &dirs).await;
    sdk.complete(CompleteRequest::new("Summarize.")).await.expect("answered");
    let account: Arc<dyn TokenSource> = Arc::new(Account::api_key("another-key"));
    sdk.complete(CompleteRequest::new("Summarize.").auth(account)).await.expect("answered");

    let key = |i: usize| {
        let requests = mock.requests();
        requests[i].headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
    };
    assert_eq!(key(0), "k", "the SDK's own credential");
    assert_eq!(key(1), "another-key", "the request's");
}

#[tokio::test]
async fn an_agents_commands_are_listed_for_a_menu() {
    let dirs = Dirs::new("commands");
    dirs.write(
        "project/.claude/skills/review/SKILL.md",
        "---\ndescription: Review a pull request\nargument-hint: \"[pr number]\"\n---\nReview $ARGUMENTS.",
    );
    dirs.write(
        "home/.claude/commands/standup.md",
        "---\ndescription: Write my standup\ndisable-model-invocation: true\n---\nWrite it.",
    );
    dirs.write(
        "project/.claude/skills/internal/SKILL.md",
        "---\ndescription: For the model only\nuser-invocable: false\n---\nDo it.",
    );
    dirs.write("plugins/eos/.claude-plugin/plugin.json", r#"{"name":"eos"}"#);
    dirs.write("plugins/eos/skills/author/SKILL.md", "---\ndescription: Build an Eos plugin\n---\nBuild.");
    let mock = mock([]).await;
    let sdk = sdk(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).plugin_dirs(vec![dirs.root.join("plugins/eos")]);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");

    let commands = agent.commands();
    let find = |name: &str| -> CommandInfo {
        commands.iter().find(|c| c.name == name).cloned().unwrap_or_else(|| panic!("{name} in {commands:?}"))
    };
    let review = find("review");
    assert_eq!(review.description, "Review a pull request");
    assert_eq!(review.argument_hint.as_deref(), Some("[pr number]"));
    assert_eq!(review.source, SkillOrigin::ProjectSkill);
    assert!(review.user_invocable && review.model_invocable);

    let standup = find("standup");
    assert_eq!(standup.source, SkillOrigin::UserCommand);
    assert!(standup.user_invocable && !standup.model_invocable, "only the user runs it");
    let internal = find("internal");
    assert!(!internal.user_invocable && internal.model_invocable, "not for the menu");
    assert_eq!(find("eos:author").source, SkillOrigin::PluginSkill { plugin: "eos".into() });
    assert!(commands.iter().any(|c| c.source == SkillOrigin::Bundled), "the bundled ones are listed too");
}
