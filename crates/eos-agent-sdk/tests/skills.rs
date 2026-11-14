//! Skills through the SDK: inline `` !`cmd` `` commands run as the agent's
//! Bash calls through the permission gate, and `context: fork` skills run as
//! subagents, as Claude Code runs them.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Dirs, all_text, mock};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{BoxFuture, Credential, Permission, PermissionGate, StaticCredential, ToolCall, TurnOutcome};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, Sdk, SpawnLimits};
use eos_agent_types::StopReason;

/// Allows everything but commands containing `secret`; records what it saw.
#[derive(Default)]
struct Gate(Mutex<Vec<String>>);

impl PermissionGate for Gate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        self.0.lock().expect("lock").push(format!("{} {}", call.name, call.input.get()));
        let deny = call.input.get().contains("secret");
        Box::pin(async move { if deny { Permission::Deny("not that one".into()) } else { Permission::Allow } })
    }
}

async fn sdk(mock: &MockApi, dirs: &Dirs, gate: Arc<Gate>) -> Sdk {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .permissions(gate)
        .build()
        .await
        .expect("sdk")
}

#[tokio::test]
async fn inline_commands_run_through_the_gate() {
    let dirs = Dirs::new("skill-inline");
    dirs.write("project/marker.txt", "MARKER-42\n");
    dirs.write("project/.claude/skills/peek/SKILL.md", "---\ndescription: Peek\n---\nThe marker: !`cat marker.txt`");
    dirs.write("project/.claude/skills/leak/SKILL.md", "---\ndescription: Leak\n---\nIt: !`echo secret`");
    let mock = mock([
        Reply::tool_use("toolu_1", "Skill", r#"{"skill":"peek"}"#),
        Reply::tool_use("toolu_2", "Skill", r#"{"skill":"leak"}"#),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(Gate::default());
    let sdk = sdk(&mock, &dirs, Arc::clone(&gate)).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let second = all_text(&requests[1]);
    assert!(second.contains("The marker: MARKER-42"), "the output replaced the snippet: {second}");
    let seen = gate.0.lock().expect("lock").clone();
    assert!(seen.iter().any(|c| c == r#"Bash {"command":"cat marker.txt"}"#), "the gate saw the command: {seen:?}");
    let third = all_text(&requests[2]);
    assert!(
        third.contains("Shell command permission check failed for pattern \"!`echo secret`\": not that one"),
        "{third}"
    );

    // A typed `/peek` runs its command the same way.
    let mock = common::mock([Reply::text("ok")]).await;
    let sdk = self::sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_input("/peek").await.expect("input").expect("turn");
    assert!(all_text(&mock.requests()[0]).contains("The marker: MARKER-42"));
}

#[tokio::test]
async fn fork_skills_run_as_subagents() {
    let dirs = Dirs::new("skill-fork");
    dirs.write(
        "project/.claude/skills/survey/SKILL.md",
        "---\ndescription: Survey the repo\ncontext: fork\nagent: Explore\n---\nList what is here, $ARGUMENTS.",
    );
    let mock = mock([
        Reply::tool_use("toolu_1", "Skill", r#"{"skill":"survey","args":"briefly"}"#),
        Reply::text("three files"),
        Reply::text("done"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let child = all_text(&requests[1]);
    assert!(child.contains("List what is here, briefly."), "the skill is the subagent's prompt: {child}");
    assert!(common::system_text(&requests[1]).contains("file search specialist"), "an Explore agent");
    let back = all_text(&requests[2]);
    assert!(back.contains("Skill \"survey\" completed (forked execution).\n\nResult:\nthree files"), "{back}");
}

/// Claude Code's interactive terminal launches a fork the Skill tool loads
/// in the background too, named as a typed one; without background tasks it
/// stays in the foreground.
#[tokio::test]
async fn an_interactive_agents_skill_tool_launches_forks_in_the_background() {
    for background in [true, false] {
        let dirs = Dirs::new("skill-fork-tool-bg");
        dirs.write(
            "project/.claude/skills/survey/SKILL.md",
            "---\ndescription: Survey\ncontext: fork\n---\nLook around.",
        );
        let mock = mock([
            Reply::tool_use("toolu_1", "Skill", r#"{"skill":"survey"}"#),
            Reply::text("found"),
            Reply::text("ok"),
            Reply::text("noted"),
        ])
        .await;
        let sdk = Sdk::builder()
            .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
            .base_url(mock.base_url())
            .model("mock-model")
            .claude_paths(ClaudePaths::for_home(&dirs.home()))
            .data_dir(dirs.root.join("data"))
            .spawn_limits(SpawnLimits { background, ..SpawnLimits::default() })
            .build()
            .await
            .expect("sdk");
        let options = AgentOptions::new(dirs.project()).mcp(false).interactive(true);
        let (agent, _events) = sdk.spawn(options).await.expect("spawn");
        agent.send_text("go").await.expect("alive").expect("turn");
        common::settle(&agent).await;
        let texts: Vec<String> = mock.requests().iter().map(all_text).collect();
        let launched = "Skill \"survey\" launched (forked execution, running in the background).\n\nRunning in the \
                        background as @survey";
        let completed = "Skill \"survey\" completed (forked execution).\n\nResult:\nfound";
        let want = if background { launched } else { completed };
        assert!(texts.iter().any(|t| t.contains(want)), "{background}: {texts:?}");
    }
}

#[tokio::test]
async fn a_typed_fork_skill_runs_as_a_subagent_without_a_parent_request() {
    let dirs = Dirs::new("skill-fork-typed");
    dirs.write("project/CLAUDE.md", "Be brief.");
    dirs.write(
        "project/.claude/skills/survey/SKILL.md",
        "---\ndescription: Survey the repo\ncontext: fork\nagent: Explore\n---\nList what is here, $ARGUMENTS.",
    );
    let mock = mock([Reply::text("three files"), Reply::text("noted")]).await;
    let sdk = sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    let out = agent.send_input("/survey  briefly ").await.expect("input").expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "only the subagent asked the model");
    assert!(all_text(&requests[0]).contains("List what is here, briefly."), "the skill is the subagent's prompt");
    assert!(common::system_text(&requests[0]).contains("file search specialist"), "an Explore agent");

    // The next turn carries the typed line and the reply, in the first message
    // after the session context.
    agent.send_text("thanks").await.expect("alive").expect("turn");
    let next = mock.requests()[1].json();
    let first: Vec<&str> =
        next["messages"][0]["content"].as_array().expect("blocks").iter().filter_map(|b| b["text"].as_str()).collect();
    assert_eq!(
        first[first.len() - 2..],
        ["/survey briefly", "<local-command-stdout>three files</local-command-stdout>"],
        "{first:?}"
    );
    assert!(first[0].starts_with("<system-reminder>"), "the session context leads: {first:?}");
    assert!(all_text(&mock.requests()[1]).contains("thanks"));
}

#[tokio::test]
async fn an_interrupt_stops_a_typed_fork() {
    let dirs = Dirs::new("skill-fork-stop");
    dirs.write("project/.claude/skills/survey/SKILL.md", "---\ndescription: Survey\ncontext: fork\n---\nLook around.");
    let mock = mock([Reply::text("slow").slowed(Duration::from_millis(400)), Reply::text("ok")]).await;
    let sdk = sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    let agent = Arc::new(agent);
    let running = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move { agent.send_input("/survey").await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    agent.interrupt();
    let out = running.await.expect("join").expect("input").expect("turn");
    assert_eq!(out, TurnOutcome::Interrupted);
    common::settle(&agent).await;

    agent.send_text("again").await.expect("alive").expect("turn");
    let text = all_text(mock.requests().last().expect("request"));
    let (line, stop) = (text.find("/survey\n"), text.find("[Request interrupted by user]"));
    assert!(line.is_some_and(|l| stop.is_some_and(|s| l < s)), "the typed line, then the interruption: {text}");
}

/// Claude Code's interactive terminal launches a typed fork in the
/// background: the turn records the launch, and the subagent's result wakes
/// the agent later as a task notification.
#[tokio::test]
async fn an_interactive_agent_launches_a_typed_fork_in_the_background() {
    let dirs = Dirs::new("skill-fork-background");
    dirs.write(
        "project/.claude/skills/survey/SKILL.md",
        "---\ndescription: Survey the repo\ncontext: fork\nagent: Explore\n---\nList what is here, $ARGUMENTS.",
    );
    let mock = mock([Reply::text("three files").slowed(Duration::from_millis(200)), Reply::text("noted")]).await;
    let sdk = sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).interactive(true);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    let out = agent.send_input("/survey briefly").await.expect("input").expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert_eq!(agent.running_subagents(), 1, "the fork works on after the turn");
    common::settle(&agent).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "the subagent, then the agent woken by its notification");
    assert!(all_text(&requests[0]).contains("List what is here, briefly."));
    let woke = all_text(&requests[1]);
    let id_at = woke.find("\"agentId\":\"").expect("the launch record") + "\"agentId\":\"".len();
    let id = &woke[id_at..id_at + 17];
    let launch = format!(
        "<local-command-stdout>Running in the background as @survey</local-command-stdout>\n\
         <forked-skill-launch>{{\"agentId\":\"{id}\",\"skillName\":\"survey\",\"description\":\"/survey \
         briefly\"}}</forked-skill-launch>"
    );
    let blocks = text_blocks(&requests[1]);
    let at = blocks.iter().position(|b| *b == launch).expect("the launch record");
    assert_eq!(blocks[at - 1], "/survey briefly", "{blocks:?}");
    let notification = format!(
        "<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n<summary>Agent \"/survey \
         briefly\" finished</summary>"
    );
    assert!(woke.contains(&notification), "no tool call is named: {woke}");
    assert!(woke.contains("<result>three files</result>"), "{woke}");
}

/// A fork of the same skill still working, or a skill with `background:
/// false`, runs the typed command in the foreground, as in Claude Code.
#[tokio::test]
async fn an_interactive_agent_runs_some_typed_forks_in_the_foreground() {
    let dirs = Dirs::new("skill-fork-foreground");
    dirs.write("project/.claude/skills/survey/SKILL.md", "---\ndescription: Survey\ncontext: fork\n---\nLook around.");
    dirs.write(
        "project/.claude/skills/quick/SKILL.md",
        "---\ndescription: Quick\ncontext: fork\nbackground: false\n---\nBe quick.",
    );
    let mock = mock([
        Reply::text("first").slowed(Duration::from_millis(600)),
        Reply::text("second"),
        Reply::text("quick one"),
        Reply::text("noted"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, Arc::new(Gate::default())).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).interactive(true);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_input("/survey").await.expect("input").expect("turn");
    tokio::time::sleep(Duration::from_millis(100)).await;
    agent.send_input("/survey again").await.expect("input").expect("turn");
    agent.send_input("/quick").await.expect("input").expect("turn");
    common::settle(&agent).await;

    let blocks = text_blocks(mock.requests().last().expect("request"));
    let after = |line: &str| blocks.iter().position(|b| b == line).map(|i| blocks[i + 1].as_str());
    assert!(after("/survey").is_some_and(|o| o.contains("Running in the background as @survey")), "{blocks:?}");
    assert_eq!(after("/survey again"), Some("<local-command-stdout>second</local-command-stdout>"));
    assert_eq!(after("/quick"), Some("<local-command-stdout>quick one</local-command-stdout>"));
    assert!(blocks.iter().any(|b| b.contains("<result>first</result>")), "{blocks:?}");
}

/// Every text block of a request's messages, in order.
fn text_blocks(request: &eos_agent_mock_api::Recorded) -> Vec<String> {
    let json = request.json();
    let messages = json["messages"].as_array().cloned().unwrap_or_default();
    messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter_map(|b| b["text"].as_str().map(str::to_owned))
        .collect()
}

/// Without background tasks, Claude Code's interactive terminal runs a typed
/// fork in the foreground too.
#[tokio::test]
async fn without_background_tasks_a_typed_fork_runs_in_the_foreground() {
    let dirs = Dirs::new("skill-fork-no-bg");
    dirs.write("project/.claude/skills/survey/SKILL.md", "---\ndescription: Survey\ncontext: fork\n---\nLook around.");
    let mock = mock([Reply::text("three files"), Reply::text("noted")]).await;
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .spawn_limits(SpawnLimits { background: false, ..SpawnLimits::default() })
        .build()
        .await
        .expect("sdk");
    let options = AgentOptions::new(dirs.project()).mcp(false).interactive(true);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_input("/survey").await.expect("input").expect("turn");
    assert_eq!(agent.running_subagents(), 0, "the turn waited for the fork");
    agent.send_text("thanks").await.expect("alive").expect("turn");
    let blocks = text_blocks(mock.requests().last().expect("request"));
    let at = blocks.iter().position(|b| b == "/survey").expect("the typed line");
    assert_eq!(blocks[at + 1], "<local-command-stdout>three files</local-command-stdout>");
}

/// A background fork's name frees once its agent left the task list, as
/// Claude Code's `allocateName` frees it (the 30 s shortened here).
#[tokio::test]
async fn a_background_forks_name_frees_when_it_leaves_the_list() {
    let dirs = Dirs::new("skill-fork-names");
    dirs.write("project/.claude/skills/survey/SKILL.md", "---\ndescription: Survey\ncontext: fork\n---\nLook around.");
    let mock = mock((0..6).map(|i| Reply::text(if i % 2 == 0 { "found" } else { "noted" }))).await;
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .task_list_retention(Duration::from_millis(300))
        .build()
        .await
        .expect("sdk");
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).interactive(true)).await.expect("spawn");
    agent.send_input("/survey").await.expect("input").expect("turn");
    common::settle(&agent).await;
    agent.send_input("/survey").await.expect("input").expect("turn");
    common::settle(&agent).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    agent.send_input("/survey").await.expect("input").expect("turn");
    common::settle(&agent).await;

    let launched: Vec<String> = text_blocks(mock.requests().last().expect("request"))
        .into_iter()
        .filter_map(|b| b.split_once("Running in the background as @").map(|(_, rest)| rest.to_owned()))
        .map(|rest| rest.split('<').next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(launched, ["survey", "survey-2", "survey"], "the second came while the first was still listed");
}
