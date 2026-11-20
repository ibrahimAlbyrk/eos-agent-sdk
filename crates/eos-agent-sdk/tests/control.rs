//! What a host changes about a live agent: model and effort, where its
//! conversation ends, its background tasks.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{Dirs, all_text, builder, mock, sdk, sdk_with_model, settle, system_text};
use eos_agent_core::{
    BackgroundTask, BackgroundTasks, BoxFuture, RewindError, TaskStatus, TokenSource, Tool, ToolContext, ToolOutput,
    ToolSpec,
};
use eos_agent_engine::JsonlStore;
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{
    Account, AgentOptions, ControlError, HostTool, ModelChange, Provider, SdkError, StopTaskError, TooLong,
};
use serde_json::value::RawValue;

#[tokio::test]
async fn a_model_change_holds_for_later_turns_and_new_subagents() {
    let dirs = Dirs::new("ctl-model");
    let call = r#"{"description":"Helper","prompt":"Help","run_in_background":false}"#;
    let mock = mock([
        Reply::text("one"),
        Reply::text("two"),
        Reply::tool_use("toolu_a", "Agent", call),
        Reply::text("helped"),
        Reply::text("three"),
    ])
    .await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let options = AgentOptions::new(dirs.project()).mcp(false).max_tokens(9_000);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    assert_eq!(agent.model().as_deref(), Some("claude-opus-5-5"));

    agent.set_model("haiku").await.expect("changed");
    assert_eq!(agent.model().as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(sdk.agents()[0].model, "claude-haiku-5-5");
    agent.send_text("second").await.expect("alive").expect("turn");
    agent.send_text("third").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let models: Vec<String> =
        requests.iter().map(|r| r.json()["model"].as_str().unwrap_or_default().to_owned()).collect();
    assert_eq!(models[0], "claude-opus-5-5");
    assert!(models[1..].iter().all(|m| m == "claude-haiku-5-5"), "later turns and the new subagent: {models:?}");
    assert_ne!(system_text(&requests[0]), system_text(&requests[1]), "Claude Code words the prompt for the model");
    assert_eq!(requests[1].json()["max_tokens"], 9_000, "what the host set at spawn still holds");
    let kept = requests[1].json()["messages"].as_array().map(Vec::len);
    assert_eq!(kept, Some(4), "the same conversation goes on: context note, two turns, the new message");

    let other = agent.set_model("gpt-5").await.expect_err("a provider with no credential here");
    assert!(matches!(other, ControlError::Sdk(SdkError::NoProvider { provider: Provider::OpenAi, .. })), "{other:?}");
    assert_eq!(agent.model().as_deref(), Some("claude-haiku-5-5"));
}

#[tokio::test]
async fn a_conversation_too_long_for_the_new_model_is_refused_or_summarized_and_a_stopped_summary_changes_nothing() {
    let dirs = Dirs::new("ctl-model-too-long");
    let mock = mock([
        Reply::text("one"),
        Reply::text("a summary never finished").slowed(Duration::from_secs(10)),
        Reply::text("the summary"),
        Reply::text("two"),
    ])
    .await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).auto_compact(false)).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    // The host knows the new model's window: far smaller than the conversation.
    let small = || ModelChange { context_window: Some(10), ..ModelChange::default() };

    let refused = agent
        .set_model_with("haiku", ModelChange { too_long: TooLong::Refuse, ..small() })
        .await
        .expect_err("too long, and not to be summarized");
    assert!(matches!(refused, ControlError::TooLong { fits: 10, .. }), "{refused:?}");
    assert_eq!(agent.model().as_deref(), Some("claude-opus-5-5"));
    assert_eq!(mock.requests().len(), 1, "nothing was summarized");

    let stop = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        agent.interrupt();
    };
    let (stopped, ()) = tokio::join!(agent.set_model_with("haiku", small()), stop);
    assert!(matches!(stopped, Err(ControlError::NotSummarized)), "{stopped:?}");
    assert_eq!(agent.model().as_deref(), Some("claude-opus-5-5"), "a stopped summary leaves the model");

    assert!(agent.set_model_with("haiku", small()).await.expect("summarized, then changed"));
    assert_eq!(agent.model().as_deref(), Some("claude-haiku-5-5"));
    agent.send_text("second").await.expect("alive").expect("turn");
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[2].json()["model"], "claude-opus-5-5", "the model it is on writes the summary");
    assert_eq!(requests[3].json()["model"], "claude-haiku-5-5");
    assert!(all_text(&requests[3]).contains("the summary"), "the new model goes on from the summary");
}

#[tokio::test]
async fn a_model_change_keeps_the_agents_own_credential() {
    let dirs = Dirs::new("ctl-model-auth");
    let mock = mock([Reply::text("one"), Reply::text("two")]).await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let own: Arc<dyn TokenSource> = Arc::new(Account::api_key("worker-key"));
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).auth(own)).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    agent.set_model("haiku").await.expect("changed");
    agent.send_text("second").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let keys: Vec<&str> =
        requests.iter().map(|r| r.headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or_default()).collect();
    assert_eq!(keys, ["worker-key", "worker-key"]);
    assert_eq!(requests[1].json()["model"], "claude-haiku-5-5");
}

#[tokio::test]
async fn an_effort_change_holds_until_changed_again() {
    let dirs = Dirs::new("ctl-effort");
    let mock = mock([Reply::text("a"), Reply::text("b"), Reply::text("c"), Reply::text("d"), Reply::text("e")]).await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("1").await.expect("alive").expect("turn");
    agent.set_effort(Some("low")).await.expect("changed");
    assert_eq!(agent.effort().as_deref(), Some("low"));
    agent.send_text("2").await.expect("alive").expect("turn");
    agent.send_text("3").await.expect("alive").expect("turn");
    agent.set_model("sonnet").await.expect("changed");
    agent.send_text("4").await.expect("alive").expect("turn");
    agent.set_effort(None).await.expect("changed");
    agent.send_text("5").await.expect("alive").expect("turn");

    let efforts: Vec<String> = mock
        .requests()
        .iter()
        .map(|r| r.json()["output_config"]["effort"].as_str().unwrap_or_default().to_owned())
        .collect();
    let default = efforts[0].clone();
    assert_ne!(default, "low");
    assert_eq!(efforts[1..4], ["low", "low", "low"], "through later turns and a model change");
    assert_eq!(efforts[4], default, "back to the model's default");
}

#[tokio::test]
async fn a_conversation_can_be_rewound_and_forked() {
    let dirs = Dirs::new("ctl-rewind");
    let mock = mock([
        Reply::text("one"),
        Reply::text("two"),
        Reply::text("three"),
        Reply::text("again"),
        Reply::text("forked"),
        Reply::text("fresh"),
    ])
    .await;
    let store = Arc::new(JsonlStore::new(dirs.root.join("transcripts")));
    let sdk = builder(&mock, &dirs).store(Arc::clone(&store) as _).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    for prompt in ["first", "second", "third"] {
        agent.send_text(prompt).await.expect("alive").expect("turn");
    }
    let before = agent.transcript().await.expect("transcript");
    // The first message, the context note, then a reply and a prompt per turn.
    assert_eq!(before.len(), 7);
    assert!(before[3].text().contains("second"));

    // Back to before the second prompt.
    agent.rewind(3).await.expect("rewound");
    assert_eq!(agent.transcript().await.expect("transcript").len(), 3);
    assert_eq!(store.load(agent.id()).await.expect("stored").len(), 3, "the store forgets them too");
    agent.send_text("second, differently").await.expect("alive").expect("turn");
    let requests = mock.requests();
    let text = all_text(&requests[3]);
    assert!(text.contains("first") && text.contains("second, differently"));
    assert!(!text.contains("third") && !text.contains("two"), "{text}");

    // Another agent from the first turn; this one goes on unchanged.
    let (fork, _events) = agent.fork_at(3, AgentOptions::new(dirs.project()).mcp(false)).await.expect("fork");
    assert_ne!(fork.id(), agent.id());
    fork.send_text("on a branch").await.expect("alive").expect("turn");
    let branch = all_text(&mock.requests()[4]);
    assert!(branch.contains("first") && branch.contains("on a branch") && !branch.contains("differently"));
    assert_eq!(agent.transcript().await.expect("transcript").len(), 5);

    // Rewound to nothing, it opens like a new conversation.
    agent.rewind(0).await.expect("rewound");
    agent.send_text("from the top").await.expect("alive").expect("turn");
    let fresh = mock.requests()[5].json();
    let messages = fresh["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "the prompt with its context, and the context note");
    assert!(all_text(&mock.requests()[5]).contains("# Environment"));

    let far = agent.rewind(99).await.expect_err("past the end");
    assert!(matches!(far, ControlError::Rewind(RewindError::OutOfRange { len: 3 })), "{far:?}");
}

#[tokio::test]
async fn a_rewind_never_keeps_a_tool_call_without_its_result() {
    let dirs = Dirs::new("ctl-rewind-tools");
    let mock = mock([Reply::tool_use("toolu_g", "Glob", r#"{"pattern":"*.md"}"#), Reply::text("none")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("look").await.expect("alive").expect("turn");
    // Prompt, context note, the call, its result, the reply.
    assert_eq!(agent.transcript().await.expect("transcript").len(), 5);
    let split = agent.rewind(3).await.expect_err("between a call and its result");
    assert!(matches!(split, ControlError::Rewind(RewindError::SplitsToolCall { keep: 3 })), "{split:?}");
    agent.rewind(4).await.expect("after the result");
    agent.rewind(2).await.expect("before the call");
}

/// Starts a background task of its own, as the Workflow tool does.
struct Runner(ToolSpec);

impl Tool for Runner {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    fn call<'a>(&'a self, _input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let task = Arc::new(BackgroundTask::new("w123", "local_workflow", "nightly-sync"));
            ctx.extensions.get_or_default::<BackgroundTasks>().insert(Arc::clone(&task));
            tokio::spawn(async move {
                task.stop_requested().cancelled().await;
                task.finish(TaskStatus::Killed);
            });
            ToolOutput::text("started w123")
        })
    }
}

#[tokio::test]
async fn the_host_stops_background_tasks_by_id() {
    let dirs = Dirs::new("ctl-stop");
    let sleep = r#"{"command":"sleep 30","run_in_background":true}"#;
    let helper = r#"{"description":"Slow helper","prompt":"Take your time","run_in_background":true}"#;
    // By whose request it is: the helper's must never take the parent's replies.
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&seen);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |body| {
        let body: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        log.lock().expect("lock").push(body["messages"].to_string());
        if body["messages"][0].to_string().contains("Take your time") {
            return Reply::text("slowly").slowed(Duration::from_secs(20));
        }
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let blocks = messages.iter().filter_map(|m| m["content"].as_array()).flatten();
        match blocks.filter(|b| b["type"] == "tool_result").count() {
            0 => Reply::tool_use("toolu_b", "Bash", sleep),
            1 => Reply::tool_use("toolu_w", "run_job", "{}"),
            2 => Reply::tool_use("toolu_a", "Agent", helper),
            3 => Reply::text("all started"),
            _ => Reply::text("noted"),
        }
    })
    .await
    .expect("mock");
    let sdk = sdk(&mock, &dirs).await;
    let runner: Arc<dyn Tool> = Arc::new(Runner(ToolSpec::new("run_job", "Runs a job.", serde_json::json!({}))));
    let options = AgentOptions::new(dirs.project()).mcp(false).tools(vec![HostTool::new(runner)]);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("start things").await.expect("alive").expect("turn");

    // The shell task's id is in the tool result the model got.
    let launched = seen.lock().expect("lock")[1].clone();
    let id = launched.split("Command running in background with ID: ").nth(1).expect("launched").split('.').next();
    let shell = id.expect("id").to_owned();
    let stopped = agent.stop_task(&shell).await.expect("stopped");
    assert_eq!((stopped.task_type.as_str(), stopped.description.as_str()), ("local_bash", "sleep 30"));
    assert_eq!(stopped.agent_id, agent.id());
    let again = agent.stop_task(&shell).await.expect_err("already stopped");
    assert_eq!(again, StopTaskError::NotRunning { id: shell.clone(), status: "killed" });
    assert_eq!(again.to_string(), format!("Task {shell} is not running (status: killed)"), "TaskStop's words");

    let job = agent.stop_task("w123").await.expect("stopped");
    assert_eq!((job.task_type.as_str(), job.description.as_str()), ("local_workflow", "nightly-sync"));

    let child = sdk.agent_tree(agent.id()).into_iter().find(|a| a.parent_id.is_some()).expect("subagent");
    assert_eq!(agent.running_subagents(), 1);
    let sub = sdk.stop_task(agent.id(), &child.agent_id).await.expect("stopped");
    assert_eq!((sub.task_type.as_str(), sub.description.as_str()), ("local_agent", "Slow helper"));
    settle(&agent).await;
    assert_eq!(agent.running_subagents(), 0);
    let told = seen.lock().expect("lock").concat();
    assert!(told.contains("<status>killed</status>") || told.contains("was stopped"), "its parent hears of it");

    assert_eq!(agent.stop_task("b00000000").await, Err(StopTaskError::NotFound("b00000000".into())));
}
