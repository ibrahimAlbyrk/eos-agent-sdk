use std::path::Path;
use std::sync::{Arc, Mutex};

use eos_agent_agents::{
    AgentDefinitions, AgentTool, ForkedAgent, ListAgentsTool, SearchTools, SendMessageTool, Source, SpawnLimits,
    agent_roots, builtin_definitions, discover,
};
use eos_agent_core::{
    BoxFuture, CancellationToken, Extensions, SubagentInfo, SubagentReport, SubagentRequest, SubagentStart, Subagents,
    Tool, ToolContext, ToolOutput,
};
use eos_agent_types::{ContentBlock, ToolResultContent, Usage};
use serde_json::value::RawValue;

#[derive(Default)]
struct FakeSubagents {
    requests: Mutex<Vec<SubagentRequest>>,
    sends: Mutex<Vec<(String, String, String)>>,
    agents: Vec<SubagentInfo>,
    reply: Option<String>,
    fail: Option<String>,
    depth: usize,
}

impl Subagents for FakeSubagents {
    fn start(&self, request: SubagentRequest) -> BoxFuture<'_, Result<SubagentStart, String>> {
        let background = request.background;
        self.requests.lock().expect("lock").push(request);
        let result = match (&self.fail, background) {
            (Some(e), _) => Err(e.clone()),
            (None, true) => Ok(SubagentStart::Launched { agent_id: "a1b2".into() }),
            (None, false) => Ok(SubagentStart::Finished(SubagentReport {
                agent_id: "a1b2".into(),
                text: self.reply.clone().unwrap_or_default(),
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    cache_read_input_tokens: Some(100),
                    ..Usage::default()
                },
                tool_uses: 3,
                duration_ms: 1234,
                structured: None,
                worktree: None,
            })),
        };
        Box::pin(async move { result })
    }

    fn send<'a>(&'a self, from: &'a str, to: &'a str, message: String) -> BoxFuture<'a, Result<(), String>> {
        self.sends.lock().expect("lock").push((from.into(), to.into(), message));
        let result = self.fail.clone().map_or(Ok(()), Err);
        Box::pin(async move { result })
    }

    fn list<'a>(&'a self, _caller: &'a str) -> BoxFuture<'a, Vec<SubagentInfo>> {
        let agents = self.agents.clone();
        Box::pin(async move { agents })
    }

    fn depth(&self, _agent: &str) -> usize {
        self.depth
    }
}

fn definitions() -> Arc<AgentDefinitions> {
    Arc::new(AgentDefinitions::new(builtin_definitions(SearchTools::Dedicated)))
}

async fn call(tool: &dyn Tool, input: &str, extensions: &Extensions) -> ToolOutput {
    let raw = RawValue::from_string(input.to_owned()).expect("json");
    let cancel = CancellationToken::new();
    let ctx = ToolContext { agent_id: "parent", tool_use_id: "toolu_1", cancel: &cancel, extensions, transcript: &[] };
    tool.call(&raw, ctx).await
}

fn texts(output: &ToolOutput) -> Vec<&str> {
    match &output.content {
        ToolResultContent::Text(t) => vec![t.as_str()],
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

#[tokio::test]
async fn agent_maps_input_and_reports_sync_result() {
    let fake = Arc::new(FakeSubagents { reply: Some("Done: 3 files.".into()), ..Default::default() });
    let tool = AgentTool::new(fake.clone(), definitions());
    assert_eq!(tool.spec().name(), "Agent");
    let out = call(
        &tool,
        r#"{"description":"  Find   the bug ","prompt":"Look in src/","subagent_type":"general_purpose",
            "model":"haiku","run_in_background":false,"name":"bug-hunt","effort":"low","isolation":"worktree"}"#,
        &Extensions::default(),
    )
    .await;
    assert!(!out.is_error);
    assert_eq!(
        texts(&out),
        [
            "Done: 3 files.",
            "agentId: a1b2 (use SendMessage with to: 'a1b2', summary: '<5-10 word recap>' to continue this agent)\n\
             <usage>subagent_tokens: 115\ntool_uses: 3\nduration_ms: 1234</usage>"
        ]
    );
    let requests = fake.requests.lock().expect("lock");
    let r = &requests[0];
    assert_eq!(
        (r.parent_id.as_str(), r.description.as_str(), r.prompt.as_str()),
        ("parent", "Find the bug", "Look in src/")
    );
    assert_eq!(r.agent_type.as_deref(), Some("general-purpose"));
    assert_eq!((r.model.as_deref(), r.name.as_deref()), (Some("haiku"), Some("bug-hunt")));
    assert!(!r.background && !r.fork);
}

#[tokio::test]
async fn explore_and_empty_reports() {
    let fake = Arc::new(FakeSubagents::default());
    let tool = AgentTool::new(fake.clone(), definitions());
    let out = call(
        &tool,
        r#"{"description":"d","prompt":"p","subagent_type":"Explore","run_in_background":false}"#,
        &Extensions::default(),
    )
    .await;
    assert_eq!(texts(&out), ["(Subagent completed but returned no output.)"]);
}

#[tokio::test]
async fn background_by_default() {
    let fake = Arc::new(FakeSubagents::default());
    let tool = AgentTool::new(fake.clone(), definitions());
    let out = call(&tool, r#"{"description":"Audit","prompt":"p"}"#, &Extensions::default()).await;
    let text = texts(&out)[0];
    assert!(
        text.starts_with("Async agent launched successfully. (This tool result is internal metadata — never quote")
    );
    assert!(text.contains("\nagentId: a1b2 (internal ID - do not mention to user. Use SendMessage with to: 'a1b2', summary: '<5-10 word recap>' to continue this agent.)\nThe agent is working in the background."));
    assert!(text.ends_with("If the user asks for progress, say the agent is still running."));
    let r = &fake.requests.lock().expect("lock")[0];
    assert!(r.background);
    assert_eq!(r.agent_type.as_deref(), Some("general-purpose"));
}

/// `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`: every agent runs in the
/// foreground, and the tool's text and schema say nothing of backgrounds.
#[tokio::test]
async fn without_background_agents_everything_runs_in_the_foreground() {
    let fake = Arc::new(FakeSubagents { reply: Some("ok".into()), ..Default::default() });
    let limits = SpawnLimits { background: false, ..SpawnLimits::default() };
    let tool = AgentTool::new(fake.clone(), definitions()).with_limits(limits);
    let out =
        call(&tool, r#"{"description":"Audit","prompt":"p","run_in_background":true}"#, &Extensions::default()).await;
    assert_eq!(texts(&out)[0], "ok");
    assert!(!fake.requests.lock().expect("lock")[0].background);

    let spec: serde_json::Value = serde_json::from_slice(tool.spec().json()).expect("json");
    let on: serde_json::Value =
        serde_json::from_slice(AgentTool::new(fake.clone(), definitions()).spec().json()).expect("json");
    assert!(on["input_schema"]["properties"].get("run_in_background").is_some());
    assert!(spec["input_schema"]["properties"].get("run_in_background").is_none());
    let (text, before) = (spec["description"].as_str().expect("text"), on["description"].as_str().expect("text"));
    assert!(before.contains("\n- Subagents run in the background by default;"));
    assert!(!text.contains("background"), "{text}");
    assert!(
        text.ends_with(
            "\n- `isolation: \"worktree\"` gives the agent its own git worktree (auto-cleaned if unchanged)."
        )
    );
    assert!(text.contains(
        "\n- The agent's final message is returned to you as the tool result; it is not shown to the user — relay \
         what matters.\n"
    ));
}

/// Claude Code's nesting limit: an agent at the limit may not start another.
#[tokio::test]
async fn agents_nest_only_as_deep_as_allowed() {
    let fake = Arc::new(FakeSubagents { depth: 3, ..Default::default() });
    let tool = AgentTool::new(fake.clone(), definitions());
    let out = call(&tool, r#"{"description":"d","prompt":"p"}"#, &Extensions::default()).await;
    assert!(out.is_error);
    assert_eq!(
        texts(&out),
        [
            "<tool_use_error>Subagent nesting limit reached (depth 3 of 3). Complete this task directly using your tools \
          instead of spawning another agent. If the user explicitly requested deeper nesting, ask them to raise \
          CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH.</tool_use_error>"
        ]
    );
    assert!(fake.requests.lock().expect("lock").is_empty());
    let deeper =
        AgentTool::new(fake.clone(), definitions()).with_limits(SpawnLimits { max_depth: 4, ..Default::default() });
    assert!(!call(&deeper, r#"{"description":"d","prompt":"p"}"#, &Extensions::default()).await.is_error);
}

#[test]
fn limits_read_the_environment_as_claude_code_does() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    };
    assert_eq!(SpawnLimits::from_env(env(&[])), SpawnLimits { background: true, max_depth: 3 });
    let set = SpawnLimits::from_env(env(&[
        ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", " Yes"),
        ("CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH", " 5 "),
    ]));
    assert_eq!(set, SpawnLimits { background: false, max_depth: 5 });
    for (bg, depth) in [("0", "0"), ("x", "2.5"), ("", "-1"), ("false", "3e2")] {
        let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([
            ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", bg),
            ("CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH", depth),
        ]));
        assert_eq!(SpawnLimits::from_env(env(pairs)), SpawnLimits::default(), "{bg} {depth}");
    }
}

#[tokio::test]
async fn fork_rules() {
    let fake = Arc::new(FakeSubagents::default());
    let tool = AgentTool::new(fake.clone(), definitions());
    let input = r#"{"description":"d","prompt":"p","subagent_type":"Fork","model":"opus"}"#;
    call(&tool, input, &Extensions::default()).await;
    {
        let r = &fake.requests.lock().expect("lock")[0];
        assert!(r.fork);
        assert_eq!((r.agent_type.as_deref(), r.model.as_deref()), (Some("fork"), None));
    }
    let inside = Extensions::default();
    inside.insert(ForkedAgent);
    let out = call(&tool, input, &inside).await;
    assert!(out.is_error);
    assert_eq!(
        texts(&out),
        [
            "<tool_use_error>Fork is not available inside a forked worker. Complete your task directly using your tools.</tool_use_error>"
        ]
    );
    let remote = call(
        &tool,
        r#"{"description":"d","prompt":"p","subagent_type":"fork","isolation":"remote"}"#,
        &Extensions::default(),
    )
    .await;
    assert!(texts(&remote)[0].starts_with("<tool_use_error>Fork cannot use isolation: \"remote\""));
    assert_eq!(fake.requests.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn agent_errors() {
    let fake = Arc::new(FakeSubagents::default());
    let tool = AgentTool::new(fake.clone(), definitions());
    let unknown =
        call(&tool, r#"{"description":"d","prompt":"p","subagent_type":"coder"}"#, &Extensions::default()).await;
    assert!(unknown.is_error);
    assert_eq!(
        texts(&unknown),
        [
            "<tool_use_error>Agent type 'coder' not found. Available agents: Explore, general-purpose, Plan</tool_use_error>"
        ]
    );
    let only_mine = AgentTool::new(
        fake.clone(),
        Arc::new(AgentDefinitions::new(builtin_definitions(SearchTools::Dedicated).into_iter().skip(1))),
    );
    let missing = call(&only_mine, r#"{"description":"d","prompt":"p"}"#, &Extensions::default()).await;
    assert_eq!(
        texts(&missing),
        [
            "<tool_use_error>subagent_type is required: the general-purpose agent is not available in this session. Available agents: fork, Explore, Plan</tool_use_error>"
        ]
    );
    let bad = call(&tool, r#"{"description":"d"}"#, &Extensions::default()).await;
    assert!(texts(&bad)[0].starts_with("<tool_use_error>InputValidationError: missing field `prompt`"));
    let bad_model = call(&tool, r#"{"description":"d","prompt":"p","model":"gpt"}"#, &Extensions::default()).await;
    assert!(bad_model.is_error);
    assert!(fake.requests.lock().expect("lock").is_empty());

    let failing = AgentTool::new(
        Arc::new(FakeSubagents { fail: Some("spawn failed".into()), ..Default::default() }),
        definitions(),
    );
    let out = call(&failing, r#"{"description":"d","prompt":"p"}"#, &Extensions::default()).await;
    assert_eq!(texts(&out), ["<tool_use_error>spawn failed</tool_use_error>"]);
}

fn info(id: &str, name: Option<&str>, running: bool) -> SubagentInfo {
    SubagentInfo {
        agent_id: id.into(),
        name: name.map(Into::into),
        description: "d".into(),
        agent_type: "general-purpose".into(),
        running,
        status: if running { "running" } else { "completed" },
        started_ms: 0,
        workflow: false,
        delisted: false,
    }
}

#[tokio::test]
async fn send_message_outputs() {
    let fake = Arc::new(FakeSubagents {
        agents: vec![info("a1", Some("scout"), false), info("a2", Some("scout"), true), info("a3", None, false)],
        ..Default::default()
    });
    let tool = SendMessageTool::new(fake.clone());
    assert!(tool.spec().is_deferred());
    let live =
        call(&tool, r#"{"to":"scout","message":"also check tests","summary":"ask"}"#, &Extensions::default()).await;
    assert_eq!(
        texts(&live),
        [
            r#"{"success":true,"message":"Message queued for delivery to scout at its next tool round. If it finishes first, it is resumed with the message and notifies again, so wait for that notification before resending."}"#
        ]
    );
    let resumed = call(&tool, r#"{"to":"a3","message":"continue"}"#, &Extensions::default()).await;
    assert_eq!(texts(&resumed), [r#"{"success":true,"message":"Resuming agent a3","resumedAgentId":"a3"}"#]);
    let main = call(&tool, r#"{"to":"main","message":"done"}"#, &Extensions::default()).await;
    assert_eq!(texts(&main), [r#"{"success":true,"message":"Message queued for the main conversation's next turn."}"#]);
    let protocol = call(
        &tool,
        r#"{"to":"scout","message":{"type":"shutdown_response","request_id":"r","approve":true}}"#,
        &Extensions::default(),
    )
    .await;
    assert!(
        texts(&protocol)[0].starts_with(
            r#"{"success":false,"message":"Not sent. In this session SendMessage carries only plain text"#
        )
    );
    {
        let sends = fake.sends.lock().expect("lock");
        assert_eq!(sends.len(), 3);
        assert_eq!(sends[0], ("parent".into(), "scout".into(), "also check tests".into()));
    }

    let failing = SendMessageTool::new(Arc::new(FakeSubagents {
        fail: Some("No agent named 'x' is reachable.".into()),
        ..Default::default()
    }));
    let out = call(&failing, r#"{"to":"x","message":"hi"}"#, &Extensions::default()).await;
    assert!(!out.is_error);
    assert_eq!(texts(&out), [r#"{"success":false,"message":"No agent named 'x' is reachable."}"#]);
}

#[tokio::test]
async fn list_agents_output() {
    let empty = ListAgentsTool::new(Arc::new(FakeSubagents::default()));
    assert_eq!(texts(&call(&empty, "{}", &Extensions::default()).await), ["No reachable agents."]);
    let tool = ListAgentsTool::new(Arc::new(FakeSubagents {
        agents: vec![info("a1", Some("scout"), true), info("a3", None, false)],
        ..Default::default()
    }));
    // Claude Code's rows: `name [ref]` (or the id), type, status, age.
    let out = texts(&call(&tool, "{}", &Extensions::default()).await).remove(0).to_owned();
    let rows: Vec<&str> = out.lines().collect();
    assert_eq!(rows[0], "Subagents (2):");
    let re = regex::Regex::new(r"^  scout \[[0-9a-f]{6}\]  ·  general-purpose  ·  running  ·  started \d+d ago$")
        .expect("re");
    assert!(re.is_match(rows[1]), "{}", rows[1]);
    assert!(rows[2].starts_with("  a3  ·  general-purpose  ·  completed  ·  started "), "{}", rows[2]);
    // A finished agent dropped from the task list is not listed, and its
    // printed `name [ref]` no longer addresses it; its name still does.
    let gone = SubagentInfo { delisted: true, ..info("a5", Some("old"), false) };
    let fake = Arc::new(FakeSubagents { agents: vec![gone], ..Default::default() });
    assert_eq!(
        texts(&call(&ListAgentsTool::new(fake.clone()), "{}", &Extensions::default()).await),
        ["No reachable agents."]
    );
    let send = SendMessageTool::new(fake.clone());
    let out = call(&send, r#"{"to":"old","message":"hi"}"#, &Extensions::default()).await;
    assert_eq!(texts(&out), [r#"{"success":true,"message":"Resuming agent old","resumedAgentId":"a5"}"#]);
    // A workflow's agents run outside Claude Code's task list: never listed.
    let workflow = SubagentInfo { workflow: true, ..info("a4", None, false) };
    let only = ListAgentsTool::new(Arc::new(FakeSubagents { agents: vec![workflow], ..Default::default() }));
    assert_eq!(texts(&call(&only, "{}", &Extensions::default()).await), ["No reachable agents."]);

    // `name [ref]` as printed addresses that agent.
    let fake = Arc::new(FakeSubagents {
        agents: vec![info("a1", Some("scout"), false), info("a2", Some("scout"), false)],
        ..Default::default()
    });
    let listed =
        texts(&call(&ListAgentsTool::new(fake.clone()), "{}", &Extensions::default()).await).remove(0).to_owned();
    let first = listed.lines().nth(1).and_then(|l| l.split("  ·  ").next()).expect("row").trim().to_owned();
    let send = SendMessageTool::new(fake.clone());
    let input = serde_json::json!({"to": first, "message": "hi"}).to_string();
    let out = call(&send, &input, &Extensions::default()).await;
    assert_eq!(texts(&out), [r#"{"success":true,"message":"Resuming agent scout","resumedAgentId":"a1"}"#]);
    assert_eq!(fake.sends.lock().expect("lock")[0].1, "a1");
}

/// Everything SendMessage and ListAgents tell the model holds here: only the
/// session's own agents can be listed and messaged.
#[test]
fn messaging_tools_promise_only_the_sessions_agents() {
    let fake: Arc<FakeSubagents> = Arc::new(FakeSubagents::default());
    let tools: [Box<dyn Tool>; 2] = [Box::new(SendMessageTool::new(fake.clone())), Box::new(ListAgentsTool::new(fake))];
    for tool in tools {
        let spec = String::from_utf8_lossy(tool.spec().json()).into_owned();
        for absent in [
            "teammate",
            "Teammate",
            "team-lead",
            "shutdown",
            "ross-session",
            "Remote Control",
            "cloud",
            "other local",
            "ON THIS MACHINE",
        ] {
            assert!(!spec.contains(absent), "{}: {absent}", tool.spec().name());
        }
    }
    let send = SendMessageTool::new(Arc::new(FakeSubagents::default()));
    let prompt = String::from_utf8_lossy(send.spec().json()).into_owned();
    assert!(prompt.contains("No one outside this session can be reached."));
    assert!(prompt.contains("names keep working after an agent completes (a send resumes it from its transcript)"));
}

/// Captures are local only (`fixtures/captured/`); without them this passes.
#[test]
fn captured_tool_definitions() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/captured/0003-v1_messages/request.json");
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let fake: Arc<FakeSubagents> = Arc::new(FakeSubagents::default());
    let captured: serde_json::Value = serde_json::from_str(&text).expect("fixture");
    let tools = captured["body"]["tools"].as_array().expect("tools");
    // ListAgents and SendMessage say what this engine offers: agents of the session only.
    let ours: [Box<dyn Tool>; 1] = [Box::new(AgentTool::new(fake, definitions()))];
    for tool in ours {
        let mut spec: serde_json::Value = serde_json::from_slice(tool.spec().json()).expect("spec json");
        let mut theirs = tools.iter().find(|t| t["name"] == spec["name"]).expect("captured").clone();
        // The deprecated parameters say only what holds here.
        for schema in [&mut spec["input_schema"], &mut theirs["input_schema"]] {
            for field in ["team_name", "mode"] {
                schema["properties"][field]["description"].take();
            }
        }
        assert_eq!(spec["description"], theirs["description"]);
        assert_eq!(spec["input_schema"], theirs["input_schema"]);
    }
}

#[test]
fn discovery_precedence() {
    let home = std::env::temp_dir().join(format!("eos-agents-test-{}", std::process::id()));
    let repo = home.join("code").join("repo");
    let write = |dir: &Path, file: &str, text: &str| {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join(file), text).expect("write");
    };
    let user = home.join(".claude/agents");
    write(&user, "reviewer.md", "---\nname: reviewer\ndescription: user's\n---\nUser prompt.");
    write(&user.join("nested"), "solo.md", "---\nname: solo\ndescription: nested\ntools: Read\n---\nSolo.");
    write(&user, "notes.md", "# not an agent\n");
    write(&user, "broken.md", "---\nname: broken\n---\n");
    write(
        &repo.join(".claude/agents"),
        "reviewer.md",
        "---\nname: reviewer\ndescription: project's\n---\nProject prompt.",
    );
    write(&repo.join(".claude/agents"), "Explore.md", "---\nname: Explore\ndescription: custom explore\n---\nMine.");
    write(&repo.join("pkg/.claude/agents"), "rev.md", "---\nname: reviewer\ndescription: nearest\n---\nNearest.");

    let roots = agent_roots(&home, &repo.join("pkg"), Some(&repo));
    let found = discover(&roots);
    assert_eq!(found.errors.len(), 1);
    assert!(found.errors[0].1.ends_with("is missing required 'description' in frontmatter"));
    let defs = AgentDefinitions::new(builtin_definitions(SearchTools::Shell).into_iter().chain(found.definitions));
    assert_eq!(defs.names(), ["Explore", "general-purpose", "Plan", "reviewer", "solo"]);
    let reviewer = defs.get("reviewer").expect("reviewer");
    assert_eq!((reviewer.description.as_str(), reviewer.source), ("nearest", Source::Project));
    assert_eq!(defs.get("Explore").map(|d| d.system_prompt.as_str()), Some("Mine."));
    assert!(defs.listing().contains("\n- solo: nested (Tools: Read)\n"));
    std::fs::remove_dir_all(&home).ok();
}

/// Parameters the agent tools accept but this engine does not offer are
/// refused in plain words, never silently ignored; an agent's name is held
/// to its schema and may not take a name that already addresses an agent.
#[tokio::test]
async fn unsupported_parameters_are_refused_and_names_checked() {
    let fake = Arc::new(FakeSubagents { agents: vec![info("a1", Some("scout"), true)], ..Default::default() });
    let send = SendMessageTool::new(fake.clone());
    for input in [
        r#"{"to":"scout","message":"hi","notify_when_idle":true}"#,
        r#"{"to":"scout","message":"","notify_when_idle":false}"#,
    ] {
        let out = call(&send, input, &Extensions::default()).await;
        assert!(
            texts(&out)[0]
                .starts_with(r#"{"success":false,"message":"Not sent. notify_when_idle is not available here"#)
        );
    }
    assert!(fake.sends.lock().expect("lock").is_empty(), "nothing was sent");

    let list = ListAgentsTool::new(fake.clone());
    let out = call(&list, r#"{"q":"scout"}"#, &Extensions::default()).await;
    assert_eq!(texts(&out), ["<tool_use_error>q is not available in this build; leave it unset.</tool_use_error>"]);
    let out = call(&list, r#"{"channel":" "}"#, &Extensions::default()).await;
    assert!(texts(&out)[0].starts_with("Subagents (1):"), "an empty value changes nothing");

    let agent = AgentTool::new(fake.clone(), definitions());
    let spawn = |name: &str| serde_json::json!({"description": "d", "prompt": "p", "name": name}).to_string();
    let pattern = "<tool_use_error>InputValidationError: name must start with a letter or digit and contain only \
                   letters, digits, underscores, or hyphens (max 64 chars)</tool_use_error>";
    for bad in ["-x", "two words", "ü", &"n".repeat(65)] {
        assert_eq!(texts(&call(&agent, &spawn(bad), &Extensions::default()).await), [pattern], "{bad}");
    }
    let main = call(&agent, &spawn("main"), &Extensions::default()).await;
    assert_eq!(
        texts(&main),
        ["<tool_use_error>InputValidationError: \"main\" is reserved \u{2014} SendMessage routes it to the main \
          conversation</tool_use_error>"]
    );
    for reserved in ["Main", "USER", "system", "team-lead", "a0123456789abcdef", "aworker-0123456789abcdef"] {
        let out = call(&agent, &spawn(reserved), &Extensions::default()).await;
        assert!(texts(&out)[0].contains("must not be a reserved name"), "{reserved}");
    }
    assert!(fake.requests.lock().expect("lock").is_empty(), "no refused name started an agent");
    let ok = call(&agent, &spawn("scout_2"), &Extensions::default()).await;
    assert!(!ok.is_error);
    assert_eq!(fake.requests.lock().expect("lock")[0].name.as_deref(), Some("scout_2"));
}

/// `team_name` and `mode` are deprecated and ignored, and say so: a call
/// with them starts the same agent as one without.
#[tokio::test]
async fn deprecated_agent_parameters_say_they_are_ignored() {
    let fake = Arc::new(FakeSubagents::default());
    let agent = AgentTool::new(fake.clone(), definitions());
    let schema: serde_json::Value = serde_json::from_slice(agent.spec().json()).expect("json");
    assert_eq!(schema["input_schema"]["properties"]["team_name"]["description"], "Deprecated; ignored.");
    let mode = schema["input_schema"]["properties"]["mode"]["description"].as_str().expect("mode");
    assert!(mode.starts_with("Deprecated; ignored."), "{mode}");
    let plain = r#"{"description":"d","prompt":"p","name":"helper"}"#;
    let extra = r#"{"description":"d","prompt":"p","name":"helper","team_name":"t","mode":"plan"}"#;
    assert!(!call(&agent, plain, &Extensions::default()).await.is_error);
    assert!(!call(&agent, extra, &Extensions::default()).await.is_error);
    let requests = fake.requests.lock().expect("lock");
    assert_eq!(format!("{:?}", requests[0]), format!("{:?}", requests[1]));
}
