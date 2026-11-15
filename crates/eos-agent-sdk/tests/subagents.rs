mod common;

use std::sync::Arc;

use common::{Dirs, agent, all_text, exists, mock, sdk, settle, system_text, tool_names};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::Reply;
use eos_agent_sdk::{Sdk, SpawnLimits};

#[tokio::test]
async fn sync_subagent_reports_back() {
    let dirs = Dirs::new("sub-sync");
    let call = r#"{"description":"Check the readme","prompt":"Read README.md","subagent_type":"Explore","run_in_background":false}"#;
    let mock =
        mock([Reply::tool_use("toolu_a", "Agent", call), Reply::text("README says hi"), Reply::text("done")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("look around").await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let child = &requests[1];
    assert!(system_text(child).contains("You are a file search specialist"));
    let tools = tool_names(child);
    assert!(tools.iter().any(|t| t == "Grep") && !tools.iter().any(|t| t == "Write" || t == "Agent"));
    assert!(!tools.iter().any(|t| t == "AskUserQuestion"), "nobody answers a subagent's questions");
    assert!(all_text(child).contains("Read README.md"));

    let parent_end = all_text(&requests[2]);
    assert!(parent_end.contains("README says hi"));
    assert!(!parent_end.contains("agentId:"), "Explore reports come back without the footer");
    assert_eq!(sdk.agent_count(), 2, "the finished subagent stays addressable");
    drop(agent);
    assert_eq!(sdk.agent_count(), 0, "dropping the agent drops its tree");
}

#[tokio::test]
async fn background_subagent_notifies_its_parent() {
    let dirs = Dirs::new("sub-bg");
    let call = r#"{"description":"Count files","prompt":"Count the files","subagent_type":"general-purpose"}"#;
    let mock = mock([
        Reply::tool_use("toolu_bg", "Agent", call),
        Reply::text("working on it"),
        Reply::text("working on it"),
        Reply::text("noted"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("count").await.expect("alive").expect("turn");
    settle(&agent).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 4, "parent, child and parent run, then the notification wakes the parent");
    let launched = all_text(&requests[1]) + &all_text(&requests[2]);
    assert!(launched.contains("Async agent launched successfully."));
    let woke = all_text(&requests[3]);
    assert!(woke.contains("<task-notification>"), "{woke}");
    assert!(woke.contains("<tool-use-id>toolu_bg</tool-use-id>"));
    assert!(woke.contains("<status>completed</status>"));
    assert!(woke.contains("<result>working on it</result>"));
    assert_eq!(agent.running_subagents(), 0);
}

#[tokio::test]
async fn fork_inherits_the_conversation() {
    let dirs = Dirs::new("sub-fork");
    let call =
        r#"{"description":"Audit","prompt":"Audit the branch","subagent_type":"fork","run_in_background":false}"#;
    let mock = mock([Reply::tool_use("toolu_f", "Agent", call), Reply::text("audited"), Reply::text("ok")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("please audit").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let (parent, child) = (requests[0].json(), requests[1].json());
    assert_eq!(parent["system"], child["system"], "same system prompt, so the cache carries over");
    assert_eq!(parent["tools"], child["tools"], "and the same tools");
    let parent_messages = parent["messages"].as_array().expect("messages");
    let child_messages = child["messages"].as_array().expect("messages");
    // The parent's last message carried its cache marker, which moves on.
    let shared = parent_messages.len() - 1;
    assert_eq!(&child_messages[..shared], &parent_messages[..shared], "the parent's history first");
    let seed = all_text(&requests[1]);
    assert!(seed.contains("Fork started — that fork is you."));
    assert!(seed.contains("<fork-boilerplate>") && seed.contains("Your directive: Audit the branch"));
    assert!(all_text(&requests[2]).contains("audited"));
}

#[tokio::test]
async fn send_message_resumes_a_finished_subagent() {
    let dirs = Dirs::new("sub-send");
    let start = r#"{"description":"Helper","prompt":"Say one","name":"helper","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", start),
        Reply::text("one"),
        Reply::tool_use("toolu_2", "SendMessage", r#"{"to":"helper","message":"Now say two"}"#),
        Reply::text("two"),
        Reply::text("two"),
        Reply::text("thanks"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("use a helper").await.expect("alive").expect("turn");
    settle(&agent).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 6);
    let sent = all_text(&requests[3]) + &all_text(&requests[4]);
    assert!(sent.contains(r#""resumedAgentId""#) && sent.contains("Resuming agent helper"), "{sent}");
    let resumed = if all_text(&requests[3]).contains("Now say two") { &requests[3] } else { &requests[4] };
    assert!(all_text(resumed).contains("Say one"), "the subagent continues its own conversation");
    let woke = all_text(&requests[5]);
    assert!(woke.contains("<task-notification>") && woke.contains("<tool-use-id>toolu_1</tool-use-id>"), "{woke}");
}

/// A Fable 5.1 parent (64k output, effort `high` by default) and the request
/// its subagent sends.
async fn child_request(dirs: &Dirs, call: &str, session_effort: Option<&str>) -> serde_json::Value {
    let mock = mock([Reply::tool_use("toolu_m", "Agent", call), Reply::text("child done"), Reply::text("ok")]).await;
    let sdk = common::sdk_with_model(&mock, dirs, "claude-fable-5-1").await;
    let mut options = eos_agent_sdk::AgentOptions::new(dirs.project()).mcp(false).thinking_summaries(false);
    if let Some(effort) = session_effort {
        options = options.effort(effort);
    }
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let parent = requests[0].json();
    assert_eq!(parent["model"], "claude-fable-5-1");
    assert_eq!(parent["max_tokens"], 64_000);
    requests[1].json()
}

fn description(request: &serde_json::Value, tool: &str) -> String {
    let tools = request["tools"].as_array().expect("tools");
    let found = tools.iter().find(|t| t["name"] == tool).expect("tool sent");
    found["description"].as_str().expect("description").to_owned()
}

#[tokio::test]
async fn subagent_gets_its_own_models_settings() {
    let dirs = Dirs::new("sub-model");
    let call = r#"{"description":"Look","prompt":"Look around","subagent_type":"general-purpose","model":"haiku","run_in_background":false}"#;
    let child = child_request(&dirs, call, None).await;
    assert_eq!(child["model"], "claude-haiku-5-5");
    assert_eq!(child["max_tokens"], 128_000, "Haiku 5.5's own output default, not Fable's 64k");
    assert_eq!(child["output_config"]["effort"], "medium", "Haiku's default effort, not Fable's high");
    assert_eq!(child["thinking"], serde_json::json!({"type": "adaptive", "display": "omitted"}));
    assert!(
        description(&child, "Bash").contains("Avoid using this tool to run `cat`"),
        "Haiku 5.5 gets Claude Code's classic Bash text under any parent"
    );
}

#[tokio::test]
async fn subagent_effort_precedence() {
    // The Agent call's effort wins over the agent type's and the session's.
    let dirs = Dirs::new("sub-effort-call");
    dirs.write("project/.claude/agents/thinker.md", "---\nname: thinker\ndescription: d\neffort: low\n---\nThink.");
    let call =
        r#"{"description":"Think","prompt":"p","subagent_type":"thinker","effort":"xhigh","run_in_background":false}"#;
    let child = child_request(&dirs, call, Some("medium")).await;
    assert_eq!(child["output_config"]["effort"], "xhigh");

    // Then the agent type's.
    let dirs = Dirs::new("sub-effort-def");
    dirs.write("project/.claude/agents/thinker.md", "---\nname: thinker\ndescription: d\neffort: low\n---\nThink.");
    let call = r#"{"description":"Think","prompt":"p","subagent_type":"thinker","run_in_background":false}"#;
    let child = child_request(&dirs, call, Some("medium")).await;
    assert_eq!(child["output_config"]["effort"], "low");

    // Then the session's.
    let dirs = Dirs::new("sub-effort-session");
    let call = r#"{"description":"Look","prompt":"p","subagent_type":"general-purpose","run_in_background":false}"#;
    let child = child_request(&dirs, call, Some("max")).await;
    assert_eq!(child["output_config"]["effort"], "max");
    assert_eq!(child["max_tokens"], 64_000, "the same model keeps the parent's limits");
}

#[tokio::test]
async fn subagent_on_an_older_model_gets_only_what_it_takes() {
    let dirs = Dirs::new("sub-old");
    dirs.write(
        "project/.claude/agents/legacy.md",
        "---\nname: legacy\ndescription: d\nmodel: claude-haiku-4-5-20251001\neffort: max\n---\nBe brief.",
    );
    let call = r#"{"description":"Old","prompt":"p","subagent_type":"legacy","run_in_background":false}"#;
    let child = child_request(&dirs, call, Some("xhigh")).await;
    assert_eq!(child["model"], "claude-haiku-4-5-20251001");
    assert_eq!(child["max_tokens"], 32_000);
    assert!(child.get("output_config").is_none(), "Haiku 4.5 takes no effort: {child}");
    assert_eq!(
        child["thinking"],
        serde_json::json!({"budget_tokens": 31_999, "type": "enabled", "display": "omitted"})
    );

    let dirs = Dirs::new("sub-older");
    dirs.write(
        "project/.claude/agents/oldest.md",
        "---\nname: oldest\ndescription: d\nmodel: claude-3-5-haiku-20241022\n---\nBe brief.",
    );
    let call = r#"{"description":"Old","prompt":"p","subagent_type":"oldest","run_in_background":false}"#;
    let child = child_request(&dirs, call, None).await;
    assert_eq!(child["max_tokens"], 8_192);
    assert!(child.get("thinking").is_none() && child.get("output_config").is_none(), "{child}");

    let dirs = Dirs::new("sub-opus46");
    dirs.write(
        "project/.claude/agents/o46.md",
        "---\nname: o46\ndescription: d\nmodel: claude-opus-4-6\neffort: xhigh\n---\nWork.",
    );
    let call = r#"{"description":"O","prompt":"p","subagent_type":"o46","run_in_background":false}"#;
    let child = child_request(&dirs, call, None).await;
    assert_eq!(child["output_config"]["effort"], "high", "Opus 4.6 has no xhigh");
    assert_eq!(child["thinking"]["type"], "adaptive");
}

/// Claude Code gives subagents its git snapshot too, except Explore and Plan.
#[tokio::test]
async fn subagents_get_the_git_snapshot_but_explore() {
    let dirs = Dirs::new("sub-git");
    dirs.write("project/a.txt", "a\n");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dirs.project())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    let call = |kind: &str| {
        format!(r#"{{"description":"d","prompt":"look","subagent_type":"{kind}","run_in_background":false}}"#)
    };
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", &call("general-purpose")),
        Reply::text("looked"),
        Reply::tool_use("toolu_2", "Agent", &call("Explore")),
        Reply::text("explored"),
        Reply::text("done"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("go").await.expect("alive").expect("turn");
    let requests = mock.requests();
    assert!(all_text(&requests[0]).contains("# gitStatus"), "the top-level agent");
    assert!(all_text(&requests[1]).contains("# gitStatus\nThis is the git status"), "a general-purpose subagent");
    assert!(!all_text(&requests[3]).contains("# gitStatus"), "not Explore");
}

#[tokio::test]
async fn an_interrupted_parent_stops_its_synchronous_subagent() {
    let dirs = Dirs::new("sub-abandoned");
    let call =
        r#"{"description":"Look","prompt":"Look around","subagent_type":"general-purpose","run_in_background":false}"#;
    let slow = Reply::text("slow").slowed(std::time::Duration::from_millis(400));
    let mock = mock([Reply::tool_use("toolu_1", "Agent", call), slow]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    let agent = std::sync::Arc::new(agent);
    let running = {
        let agent = std::sync::Arc::clone(&agent);
        tokio::spawn(async move { agent.send_text("go").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(agent.running_subagents(), 1);
    agent.interrupt();
    running.await.expect("join").expect("alive").expect("turn");
    assert_eq!(agent.running_subagents(), 0, "the abandoned subagent no longer counts as working");
    settle(&agent).await;
}

/// Claude Code's ListAgents shows every subagent of the session (its task
/// list), not only those the caller started, and SendMessage reaches them.
#[tokio::test]
async fn list_agents_shows_the_whole_session() {
    let dirs = Dirs::new("sub-session-list");
    let outer = r#"{"description":"Outer","prompt":"Start a helper","name":"outer","run_in_background":false}"#;
    let inner = r#"{"description":"Inner","prompt":"Say hi","name":"inner","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_o", "Agent", outer),
        Reply::tool_use("toolu_i", "Agent", inner),
        Reply::text("hi"),
        Reply::text("outer done"),
        Reply::tool_use("toolu_l", "ListAgents", "{}"),
        Reply::tool_use("toolu_s", "SendMessage", r#"{"to":"nobody","message":"x"}"#),
        Reply::text("listed"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    let requests = mock.requests();
    let listed = all_text(&requests[5]);
    assert!(listed.contains("outer [") && listed.contains("inner ["), "a nested subagent is listed too: {listed}");
    let last = all_text(requests.last().expect("requests"));
    assert!(last.contains("No agent named 'nobody' is currently addressable. Spawn a new one or use the agent ID."));
}

async fn limited_sdk(mock: &eos_agent_mock_api::MockApi, dirs: &Dirs, model: &str, limits: SpawnLimits) -> Sdk {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model(model)
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .spawn_limits(limits)
        .build()
        .await
        .expect("sdk")
}

/// Claude Code's nesting limit: a subagent at the limit may not start
/// another, and is told so in Claude Code's words.
#[tokio::test]
async fn subagents_nest_only_as_deep_as_allowed() {
    let dirs = Dirs::new("sub-depth");
    let call = r#"{"description":"Dig","prompt":"Dig deeper","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", call),
        Reply::tool_use("toolu_2", "Agent", call),
        Reply::text("did it myself"),
        Reply::text("done"),
    ])
    .await;
    let sdk = limited_sdk(&mock, &dirs, "mock-model", SpawnLimits { max_depth: 1, ..SpawnLimits::default() }).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("go").await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        all_text(&requests[2]).contains(
            "<tool_use_error>Subagent nesting limit reached (depth 1 of 1). Complete this task directly using your \
             tools instead of spawning another agent. If the user explicitly requested deeper nesting, ask them to \
             raise CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH.</tool_use_error>"
        ),
        "{}",
        all_text(&requests[2])
    );
    assert!(all_text(&requests[3]).contains("did it myself"));
}

/// `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`: subagents run in the foreground,
/// and both of Claude Code's Agent texts drop what they say of backgrounds.
#[tokio::test]
async fn without_background_tasks_subagents_run_in_the_foreground() {
    let dirs = Dirs::new("sub-no-bg");
    let call = r#"{"description":"Count files","prompt":"Count the files","run_in_background":true}"#;
    let mock = mock([Reply::tool_use("toolu_1", "Agent", call), Reply::text("12 files"), Reply::text("done")]).await;
    let off = SpawnLimits { background: false, ..SpawnLimits::default() };
    let sdk = limited_sdk(&mock, &dirs, "mock-model", off).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("count").await.expect("alive").expect("turn");
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "the parent waited for the subagent");
    let back = all_text(&requests[2]);
    assert!(back.contains("12 files") && !back.contains("Async agent launched"), "{back}");
    assert!(!description(&requests[0].json(), "Agent").contains("background"));

    let dirs = Dirs::new("sub-no-bg-full");
    let full_mock = common::mock([Reply::text("ok")]).await;
    let sdk = limited_sdk(&full_mock, &dirs, "claude-haiku-4-5", off).await;
    let (full_agent, _events) = common::agent(&sdk, &dirs).await;
    full_agent.send_text("hi").await.expect("alive").expect("turn");
    let full = description(&full_mock.requests()[0].json(), "Agent");
    assert!(full.contains("## When to fork"), "the full text: {full}");
    assert!(full.contains(
        "- When the agent is done, it will return a single message back to you. The result returned by the agent is \
         not visible to the user. To show the user the result, you should send a text message back to the user with \
         a concise summary of the result.\n- Trust but verify:"
    ));
    assert!(full.contains("before reporting the work as done.\n- To continue a previously spawned agent"), "{full}");
    assert!(!full.contains("Agents run in the background by default") && !full.contains("**Don't race**"));
}

/// Claude Code drops a finished subagent from its task list 30 s after it
/// ended (here shortened): ListAgents no longer shows it, but SendMessage
/// still resumes it by name.
#[tokio::test]
async fn a_finished_subagent_leaves_the_list_but_still_resumes() {
    let dirs = Dirs::new("sub-evict");
    let call = r#"{"description":"Scout","prompt":"Look","name":"scout","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", call),
        Reply::text("found it"),
        Reply::tool_use("toolu_2", "ListAgents", "{}"),
        Reply::text("ok"),
        Reply::tool_use("toolu_3", "ListAgents", "{}"),
        Reply::text("ok"),
        Reply::tool_use("toolu_4", "SendMessage", r#"{"to":"scout","message":"look again","summary":"again"}"#),
        Reply::text("ok"),
        Reply::text("ok"),
        Reply::text("ok"),
    ])
    .await;
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .task_list_retention(std::time::Duration::from_millis(300))
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("scout").await.expect("alive").expect("turn");
    let listed = all_text(&mock.requests()[3]);
    assert!(listed.contains("Subagents (1):\n  scout ["), "still listed right after it ended: {listed}");

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    agent.send_text("list").await.expect("alive").expect("turn");
    let later = all_text(&mock.requests()[5]);
    assert!(later.contains("No reachable agents."), "dropped from the list: {later}");

    agent.send_text("again").await.expect("alive").expect("turn");
    settle(&agent).await;
    let sent = mock.requests().iter().map(all_text).find(|t| t.contains("Resuming agent scout"));
    assert!(sent.is_some_and(|t| t.contains(r#""resumedAgentId":"a"#)), "SendMessage resumed it by name");
}

/// Claude Code's name registry holds one agent per name: a second agent
/// given a name takes it, and the first is then known by its id alone.
#[tokio::test]
async fn a_name_belongs_to_the_agent_given_it_last() {
    let dirs = Dirs::new("sub-name-latest");
    let first = r#"{"description":"First","prompt":"One","name":"helper","run_in_background":false}"#;
    let second = r#"{"description":"Second","prompt":"Two","name":"helper","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_1", "Agent", first),
        Reply::text("one"),
        Reply::tool_use("toolu_2", "Agent", second),
        Reply::text("two"),
        Reply::tool_use("toolu_3", "ListAgents", "{}"),
        Reply::tool_use("toolu_4", "SendMessage", r#"{"to":"helper","message":"again"}"#),
        Reply::text("sent"),
        Reply::text("again done"),
        Reply::text("ok"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    let texts: Vec<String> = mock.requests().iter().map(all_text).collect();
    let listed = texts.iter().find(|t| t.contains("Subagents (2):")).expect("listed");
    let rows: Vec<&str> = listed.lines().filter(|l| l.contains("  ·  general-purpose  ·  ")).collect();
    assert_eq!(rows.len(), 2, "{listed}");
    assert!(rows[0].trim_start().starts_with('a') && !rows[0].contains("helper"), "the first by its id: {listed}");
    assert!(rows[1].trim_start().starts_with("helper ["), "the second holds the name: {listed}");
    // Both ran in the foreground: only a resumed run reports. Which reply it
    // got is a race with the caller's; who reports is not.
    let resumed = texts.iter().any(|t| t.contains("Second\" finished"));
    assert!(resumed, "SendMessage by name reached the second");
}

/// A subagent's response calls Write but says `end_turn`; its parent then
/// asks it where the file is. The call runs, so the message that resumes the
/// subagent never lands between the call and its result (the API would
/// refuse that conversation, and the subagent would fail).
#[tokio::test]
async fn a_subagent_call_that_ends_its_response_still_runs_before_a_message_resumes_it() {
    use eos_agent_types::StopReason;
    let dirs = Dirs::new("sub-stop-call");
    let file = dirs.project().join("KARNE.md");
    let write = serde_json::json!({"file_path": file, "content": "the plan"}).to_string();
    let start = r#"{"description":"Plan","prompt":"Write the plan","name":"planner","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_p1", "Agent", start),
        Reply::tool_use("toolu_w", "Write", &write).stopping(Some(StopReason::EndTurn)),
        Reply::text("written"),
        Reply::tool_use("toolu_p2", "SendMessage", r#"{"to":"planner","message":"Where is the file?"}"#),
        Reply::text("fine"),
        Reply::text("fine"),
        Reply::text("fine"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("plan it").await.expect("alive").expect("turn");
    settle(&agent).await;

    assert!(exists(&file), "the call ran");
    for (i, request) in mock.requests().iter().enumerate() {
        assert_eq!(eos_agent_mock_api::pairing_violation(&request.body), None, "request {i}");
    }
    let resumed = mock
        .requests()
        .iter()
        .any(|r| all_text(r).contains("Where is the file?") && r.body.windows(7).any(|w| w == b"toolu_w"));
    assert!(resumed, "the subagent goes on with its own conversation, the call answered");
}
