mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Dirs, all_text, mock, settle, system_text, tool_names};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{AgentEvent, Credential, StaticCredential};
use eos_agent_engine::JsonlStore;
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, Sdk, SubagentEvent, SubagentEventKind, SystemPromptOverride};
use tokio::sync::mpsc;

async fn sdk_with_store(mock: &MockApi, dirs: &Dirs, idle: Duration) -> Sdk {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .store(Arc::new(JsonlStore::new(dirs.root.join("transcripts"))))
        .idle_eviction(Some(idle))
        .build()
        .await
        .expect("sdk")
}

#[tokio::test]
async fn finished_subagents_are_let_go_and_resume_from_their_transcript() {
    let dirs = Dirs::new("release");
    let start = r#"{"description":"Helper","prompt":"Say one","name":"helper","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", start),
        Reply::text("one"),
        Reply::text("started it"),
        Reply::tool_use("toolu_2", "SendMessage", r#"{"to":"helper","message":"Now say two"}"#),
        Reply::text("two"),
        Reply::text("two"),
        Reply::text("thanks"),
    ])
    .await;
    let sdk = sdk_with_store(&mock, &dirs, Duration::from_millis(50)).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("use a helper").await.expect("alive").expect("turn");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let agents = sdk.agents();
    let helper = agents.iter().find(|a| a.name.as_deref() == Some("helper")).expect("still listed");
    assert!(helper.evicted, "the finished subagent was let go");
    let root = agents.iter().find(|a| a.parent_id.is_none()).expect("root");
    assert!(root.evicted, "the idle top-level agent keeps its history on disk only");

    agent.send_text("ask it again").await.expect("alive").expect("turn");
    settle(&agent).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 7);
    let resumed = if all_text(&requests[4]).contains("Now say two") { &requests[4] } else { &requests[5] };
    assert!(all_text(resumed).contains("Say one"), "the subagent continues its own conversation");
    assert!(all_text(&requests[6]).contains("<task-notification>"));
}

#[tokio::test]
async fn subagent_events_reach_the_host_tagged_with_their_parent() {
    let dirs = Dirs::new("sub-events");
    let call =
        r#"{"description":"Check","prompt":"Read README.md","subagent_type":"Explore","run_in_background":false}"#;
    let mock =
        mock([Reply::tool_use("toolu_a", "Agent", call), Reply::text("README says hi"), Reply::text("done")]).await;
    // A model that takes an effort, so the subagent runs at one.
    let sdk = common::sdk_with_model(&mock, &dirs, "claude-fable-5-1").await;
    let (tx, mut rx) = mpsc::channel::<SubagentEvent>(1024);
    let options = AgentOptions::new(dirs.project()).mcp(false).subagent_events(tx);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("look").await.expect("alive").expect("turn");
    drop(agent);

    let mut text = String::new();
    let mut ended = false;
    let mut life = Vec::new();
    let mut efforts = Vec::new();
    while let Some(SubagentEvent { agent_id, parent_id, event }) = rx.recv().await {
        assert_ne!(&*agent_id, &*parent_id);
        assert!(!parent_id.is_empty());
        match event {
            SubagentEventKind::Agent(AgentEvent::TextDelta { text: t }) => text.push_str(&t),
            SubagentEventKind::Agent(AgentEvent::TurnEnded { .. }) => ended = true,
            SubagentEventKind::Agent(_) => {}
            SubagentEventKind::Started(about) => {
                assert!(text.is_empty(), "announced before its first event");
                life.push(format!("{} on {} by {}", about.agent_type, about.model, about.tool_use_id));
                efforts.push(about.effort.clone());
            }
            SubagentEventKind::Ended(end) => {
                assert!(ended, "closed after its last event");
                life.push(format!("{} with {:?} output tokens", end.status, end.usage.output_tokens));
            }
        }
    }
    assert_eq!(text, "README says hi");
    assert_eq!(life.len(), 2, "{life:?}");
    assert!(life[0].starts_with("Explore on ") && life[0].ends_with(" by toolu_a"), "{life:?}");
    assert!(life[1].starts_with("completed with Some("), "{life:?}");
    // The effort it was announced at is the one its requests ask for.
    let sent = mock.requests()[1].json()["output_config"]["effort"].as_str().map(str::to_owned);
    assert!(sent.is_some(), "the subagent's model takes an effort");
    assert_eq!(efforts, [sent]);
}

#[tokio::test]
async fn a_new_agent_sees_edited_instructions() {
    let dirs = Dirs::new("reread");
    dirs.write("project/CLAUDE.md", "Rule: say alpha.");
    let mock = mock([Reply::text("a"), Reply::text("b")]).await;
    let sdk = common::sdk(&mock, &dirs).await;
    let (first, _e1) = common::agent(&sdk, &dirs).await;
    first.send_text("hi").await.expect("alive").expect("turn");
    dirs.write("project/CLAUDE.md", "Rule: say beta, not alpha.");
    let (second, _e2) = common::agent(&sdk, &dirs).await;
    second.send_text("hi").await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert!(all_text(&requests[0]).contains("Rule: say alpha."));
    assert!(all_text(&requests[1]).contains("Rule: say beta, not alpha."));
}

#[tokio::test]
async fn options_replace_the_prompt_and_narrow_the_tools() {
    let dirs = Dirs::new("options");
    let mock = mock([Reply::text("a"), Reply::text("b")]).await;
    let sdk = common::sdk(&mock, &dirs).await;
    let replaced = AgentOptions::new(dirs.project())
        .mcp(false)
        .system_prompt(SystemPromptOverride::Replace("You are a test.".into()))
        .allowed_tools(vec!["Read".into(), "Grep".into(), "Bash".into()])
        .disallowed_tools(vec!["Bash".into()]);
    let (agent, _e) = sdk.spawn(replaced).await.expect("spawn");
    agent.send_text("hi").await.expect("alive").expect("turn");
    let appended = AgentOptions::new(dirs.project())
        .mcp(false)
        .system_prompt(SystemPromptOverride::Append("Always answer in French.".into()));
    let (agent, _e) = sdk.spawn(appended).await.expect("spawn");
    agent.send_text("hi").await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert_eq!(system_text(&requests[0]), "You are a test.");
    assert_eq!(tool_names(&requests[0]), ["Grep", "Read"]);
    let system = system_text(&requests[1]);
    assert!(system.starts_with("You are Claude Code") || system.contains("Claude"), "{system}");
    assert!(system.ends_with("Always answer in French."));
    assert!(tool_names(&requests[1]).len() > 10);
}
