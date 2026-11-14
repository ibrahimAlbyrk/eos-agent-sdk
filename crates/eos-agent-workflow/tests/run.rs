//! Whole runs over a fake Subagents port: the real realm, journal, progress
//! and notification, with scripted subagents.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eos_agent_core::{
    BackgroundTasks, BoxFuture, CancellationToken, Extensions, SubagentInfo, SubagentReport, SubagentRequest,
    SubagentStart, SubagentSystem, Subagents, TaskEvent, TaskEventKind, Tool, ToolContext, Workspace,
};
use eos_agent_types::{StopReason, Usage};
use eos_agent_workflow::{WORKFLOW_SUBAGENT, WorkflowConfig, WorkflowTool};
use serde_json::value::RawValue;
use serde_json::{Value, json};

/// Answers each agent by its prompt: `fail:` → error, `slow:` → never, a
/// prompt starting `{` → that JSON as the structured answer, else echoes.
#[derive(Default)]
struct Fake {
    requests: Mutex<Vec<SubagentRequest>>,
    notes: Mutex<Vec<String>>,
    events: Mutex<Vec<TaskEvent>>,
}

impl Fake {
    fn notes(&self) -> Vec<String> {
        self.notes.lock().expect("lock").clone()
    }

    fn prompts(&self) -> Vec<String> {
        self.requests.lock().expect("lock").iter().map(|r| r.prompt.clone()).collect()
    }
}

impl Subagents for Fake {
    fn start(&self, request: SubagentRequest) -> BoxFuture<'_, Result<SubagentStart, String>> {
        self.requests.lock().expect("lock").push(request.clone());
        Box::pin(async move {
            let n = self.requests.lock().expect("lock").len();
            let id = format!("a{n:016x}");
            if let Some(obs) = &request.observer {
                obs.started(&id, "claude-haiku-5-5");
                obs.event(
                    &id,
                    &eos_agent_core::AgentEvent::Usage(Box::new(eos_agent_core::ResponseUsage {
                        usage: Usage { output_tokens: Some(10), ..Usage::default() },
                        model: String::new(),
                    })),
                );
            }
            let prompt = request.prompt.clone();
            let first = self.requests.lock().expect("lock").iter().filter(|r| r.prompt == prompt).count() == 1;
            if prompt.starts_with("throttle:") && first {
                // A short reply with no stop reason, after 600 ms.
                if let Some(obs) = &request.observer {
                    let usage = Usage { output_tokens: Some(3), ..Usage::default() };
                    let usage = eos_agent_core::ResponseUsage { usage, model: String::new() };
                    obs.event(&id, &eos_agent_core::AgentEvent::Usage(Box::new(usage)));
                    let ended =
                        eos_agent_core::AgentEvent::TurnEnded { stop_reason: StopReason::EndTurn, stated: false };
                    obs.event(&id, &ended);
                }
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
            if let Some(rest) = prompt.strip_prefix("fail:") {
                return Err(format!("Agent {id} failed: {rest}"));
            }
            if prompt.starts_with("slow:") {
                std::future::pending::<()>().await;
            }
            if let Some(ms) =
                prompt.strip_prefix("wait:").and_then(|r| r.split(':').next()).and_then(|n| n.parse().ok())
            {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            let structured = request.output_schema.as_ref().map(|_| {
                let text = if prompt.starts_with('{') { prompt.clone() } else { r#"{"ok":true}"#.to_owned() };
                RawValue::from_string(text).expect("json")
            });
            Ok(SubagentStart::Finished(SubagentReport {
                agent_id: id,
                text: format!("echo:{prompt}"),
                usage: Usage::default(),
                tool_uses: 1,
                duration_ms: 5,
                structured,
                worktree: None,
            }))
        })
    }

    fn send<'a>(&'a self, _: &'a str, _: &'a str, _: String) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn list<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Vec<SubagentInfo>> {
        Box::pin(async { Vec::new() })
    }

    fn notify<'a>(&'a self, _agent: &'a str, text: String, _wake: bool) -> BoxFuture<'a, Result<(), String>> {
        self.notes.lock().expect("lock").push(text);
        Box::pin(async { Ok(()) })
    }

    fn task_event(&self, event: TaskEvent) -> BoxFuture<'_, ()> {
        self.events.lock().expect("lock").push(event);
        Box::pin(async {})
    }

    fn model_of(&self, _agent: &str) -> Option<String> {
        Some("claude-opus-5-5".into())
    }

    fn agent_types(&self, _caller: &str) -> Vec<String> {
        vec!["general-purpose".into(), "Explore".into()]
    }

    fn tool_names(&self, _caller: &str) -> Vec<String> {
        ["Bash", "Monitor", "Read", "mcp__github__list_prs"].map(String::from).to_vec()
    }
}

struct Setup {
    root: PathBuf,
    fake: Arc<Fake>,
    tool: WorkflowTool,
    ext: Extensions,
}

fn setup() -> Setup {
    let root = std::env::temp_dir().join(format!("eos-wf-run-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("proj");
    std::fs::create_dir_all(&cwd).expect("dir");
    let fake = Arc::new(Fake::default());
    let mut config = WorkflowConfig::new(root.join("data"));
    config.user_dir = Some(root.join("home"));
    config.stall = Duration::from_secs(30);
    let tool = WorkflowTool::new(config, Arc::clone(&fake) as Arc<dyn Subagents>);
    let ext = Extensions::default();
    ext.insert(Workspace::new(cwd, vec![], root.join("scratch")));
    Setup { root, fake, tool, ext }
}

impl Setup {
    async fn call(&self, input: Value) -> (String, bool) {
        let raw = RawValue::from_string(input.to_string()).expect("json");
        let cancel = CancellationToken::new();
        let ctx = ToolContext {
            agent_id: "main",
            tool_use_id: "toolu_1",
            cancel: &cancel,
            extensions: &self.ext,
            transcript: &[],
        };
        let out = self.tool.call(&raw, ctx).await;
        let text = match out.content {
            eos_agent_types::ToolResultContent::Text(t) => t,
            eos_agent_types::ToolResultContent::Blocks(_) => String::new(),
        };
        (text, out.is_error)
    }

    async fn note(&self, n: usize) -> String {
        for _ in 0..400 {
            if let Some(note) = self.fake.notes().get(n) {
                return note.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no notification {n}; events: {:?}", self.fake.events.lock().expect("lock").len());
    }

    fn session(&self) -> PathBuf {
        self.root.join("data").join("main")
    }
}

fn field<'a>(text: &'a str, tag: &str) -> &'a str {
    let open = format!("<{tag}>");
    let start = text.find(&open).map(|i| i + open.len()).unwrap_or(0);
    let end = text[start..].find(&format!("</{tag}>")).map_or(text.len(), |e| start + e);
    &text[start..end]
}

fn run_id(launch: &str) -> String {
    launch.lines().find_map(|l| l.strip_prefix("Run ID: ")).expect("run id").to_owned()
}

#[tokio::test]
async fn a_two_phase_pipeline_with_schema() {
    let s = setup();
    let script = r#"export const meta = {
  name: 'review-changes',
  description: 'Review changed files',
  phases: [{ title: 'Review' }, { title: 'Verify' }],
}
const S = {type: 'object', properties: {findings: {type: 'array', items: {type: 'string'}}}, required: ['findings']}
const results = await pipeline(
  ['bugs', 'perf'],
  d => agent(`{"findings":["${d}-1","${d}-2"]}`, {label: `review:${d}`, phase: 'Review', schema: S}),
  review => parallel(review.findings.map(f => () => agent(`verify ${f}`, {label: `verify:${f}`, phase: 'Verify'})))
)
log(`verified ${results.flat().length}`)
return { confirmed: results.flat() }
"#;
    let (launch, is_error) = s.call(json!({"script": script})).await;
    assert!(!is_error, "{launch}");
    assert!(launch.starts_with("Workflow launched in background. Task ID: w"), "{launch}");
    let id = run_id(&launch);
    assert!(id.starts_with("wf_") && id.len() == 15, "{id}");
    let note = s.note(0).await;
    assert_eq!(field(&note, "status"), "completed", "{note}");
    assert_eq!(field(&note, "summary"), "Dynamic workflow \"Review changed files\" completed");
    assert_eq!(
        field(&note, "result"),
        r#"{"confirmed":["echo:verify bugs-1","echo:verify bugs-2","echo:verify perf-1","echo:verify perf-2"]}"#
    );
    assert!(
        note.contains("<agent_count>6</agent_count><agents_done>6</agents_done><agents_error>0</agents_error>"),
        "{note}"
    );

    // Workflow subagents: their own type and prompt, the denied tools.
    let requests = s.fake.requests.lock().expect("lock").clone();
    let review = requests.iter().find(|r| r.description == "review:bugs").expect("review");
    assert_eq!(review.agent_type.as_deref(), Some(WORKFLOW_SUBAGENT));
    assert!(
        matches!(&review.system, Some(SubagentSystem::Replace(p)) if p.contains("MUST call the StructuredOutput tool"))
    );
    assert!(review.output_schema.is_some());
    assert_eq!(review.deny_tools, vec!["SendUserMessage", "Agent", "Workflow"]);
    let verify = requests.iter().find(|r| r.description == "verify:bugs-1").expect("verify");
    assert!(matches!(&verify.system, Some(SubagentSystem::Replace(p)) if p.contains("returned **verbatim**")));
    assert_eq!(verify.workflow.as_ref().map(|w| w.run_id.as_str()), Some(id.as_str()));

    // Files in Claude Code's layout.
    let session = s.session();
    let script_file = session.join("workflows/scripts").join(format!("review-changes-{id}.js"));
    assert_eq!(std::fs::read_to_string(&script_file).expect("script"), script);
    let record_text = std::fs::read_to_string(session.join(format!("workflows/{id}.json"))).expect("record");
    // Keys in Claude Code's order, not sorted.
    assert!(record_text.starts_with(r#"{"runId":"#), "{record_text}");
    assert!(
        record_text.contains(r#"{"type":"workflow_agent","index":1,"label":"review:bugs","phaseIndex":1,"#),
        "{record_text}"
    );
    let record: Value = serde_json::from_str(&record_text).expect("json");
    assert_eq!(record["status"], "completed");
    assert_eq!(record["workflowName"], "review-changes");
    assert_eq!(record["agentCount"], 6);
    assert_eq!(record["logs"], json!(["verified 4"]));
    assert_eq!(record["phases"], json!([{"title": "Review"}, {"title": "Verify"}]));
    let rows = record["workflowProgress"].as_array().expect("rows");
    assert_eq!(rows[0], json!({"type": "workflow_phase", "index": 1, "title": "Review"}));
    assert!(rows.iter().all(|r| r["type"] != "workflow_log"));
    let agents: Vec<&Value> = rows.iter().filter(|r| r["type"] == "workflow_agent").collect();
    assert_eq!(agents.len(), 6);
    assert!(
        agents.iter().all(|a| a["state"] == "done" && a["agentId"].is_string() && a["model"] == "claude-haiku-5-5")
    );
    if let Ok(out) = std::env::var("WORKFLOW_FIXTURE_OUT") {
        std::fs::copy(session.join(format!("workflows/{id}.json")), out).expect("fixture");
    }
    // The shape hosts read (Eos's parseWorkflowRecord / applyWorkflowProgress)
    // is pinned by the fixture next to these tests.
    let fixture: Value = serde_json::from_str(include_str!("fixtures/workflow-record.json")).expect("fixture");
    let keys = |v: &Value| v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    if std::env::var("WORKFLOW_FIXTURE_OUT").is_err() {
        assert_eq!(keys(&record), keys(&fixture), "record fields");
        let agent = |v: &Value| {
            v["workflowProgress"].as_array().and_then(|r| r.iter().find(|r| r["type"] == "workflow_agent")).map(keys)
        };
        assert_eq!(agent(&record), agent(&fixture), "agent row fields");
    }
    let journal =
        std::fs::read_to_string(session.join(format!("subagents/workflows/{id}/journal.jsonl"))).expect("journal");
    assert!(journal.starts_with("{\"type\":\"launched\"}\n"), "{journal}");
    assert_eq!(journal.matches("\"type\":\"result\"").count(), 6);

    // Task events: started, progress with Claude Code's rows, ended.
    let events = s.fake.events.lock().expect("lock").clone();
    assert!(matches!(&events[0].kind, TaskEventKind::Started { workflow_name: Some(n), .. } if n == "review-changes"));
    assert!(events.iter().any(|e| matches!(&e.kind, TaskEventKind::Progress { workflow_progress: Some(_), .. })));
    assert!(
        matches!(&events.last().expect("ended").kind, TaskEventKind::Ended { status, .. } if status == "completed")
    );
    std::fs::remove_dir_all(&s.root).ok();
}

#[tokio::test]
async fn failing_slots_null_out_and_errors_fail_the_run() {
    let s = setup();
    let script = "export const meta = {name: 'p', description: 'Parallel'}\n\
                  const r = await parallel([() => agent('a'), () => agent('fail:quota'), () => { throw new Error('boom') }])\n\
                  return r";
    s.call(json!({"script": script})).await;
    let note = s.note(0).await;
    assert_eq!(field(&note, "result"), r#"["echo:a",null,null]"#);
    assert_eq!(
        field(&note, "failures"),
        "[fail:quota] failed: Agent a0000000000000002 failed: quota\nparallel[2] failed: boom"
    );

    let s = setup();
    s.call(json!({"script": "export const meta = {name: 'x', description: 'Throws'}\nawait agent('a')\nthrow new TypeError('nope')"})).await;
    let note = s.note(0).await;
    assert_eq!(field(&note, "status"), "failed");
    assert!(field(&note, "summary").starts_with("Dynamic workflow \"Throws\" failed: TypeError: nope"), "{note}");
    assert!(field(&note, "recovery").starts_with("To resume after editing the script, call: Workflow({scriptPath: '"));
    std::fs::remove_dir_all(&s.root).ok();
}

#[tokio::test]
async fn validation_errors_use_claude_codes_words() {
    let s = setup();
    let (out, err) = s.call(json!({"script": "const x = 1"})).await;
    assert!(err);
    assert!(out.starts_with("<tool_use_error>Invalid workflow script: `export const meta = { name, description, phases }` must be the FIRST statement"), "{out}");
    assert!(out.contains("Load the `workflow-authoring` skill"));
    let (out, _) =
        s.call(json!({"script": "export const meta = {name: 'a', description: 'b'}\nreturn Date.now()"})).await;
    assert!(out.contains("Workflow scripts must be deterministic"), "{out}");
    let (out, _) =
        s.call(json!({"script": "export const meta = {name: 'a', description: 'b'}\nconst x: string = 1"})).await;
    assert!(out.contains("Invalid workflow script: Script parse error: "), "{out}");
    assert!(out.contains("Workflow scripts must be plain JavaScript"), "{out}");
    let (out, _) =
        s.call(json!({"script": "export const meta = {name: 'a', description: 'b'}\nawait import('fs')"})).await;
    assert!(
        out.starts_with(
            "Workflow script has a syntax error and was not launched:\nSyntaxError: import() is not available"
        ),
        "{out}"
    );
    let (out, _) = s.call(json!({"name": "missing"})).await;
    assert_eq!(out, "<tool_use_error>Workflow \"missing\" not found. Available: (none)</tool_use_error>");
    let (out, _) = s.call(json!({"scriptPath": "/etc/hosts"})).await;
    assert!(out.contains("scriptPath must be a script path this tool returned"), "{out}");
    let (out, _) = s.call(json!({"script": "x", "resumeFromRunId": "wf_abcdef12"})).await;
    assert!(out.contains("The journal for workflow run wf_abcdef12 is not on disk"), "{out}");
    let (out, _) = s.call(json!({})).await;
    assert!(out.contains("Must provide script, name, scriptPath, or runId"));
    std::fs::remove_dir_all(&s.root).ok();
}

#[tokio::test]
async fn disallowed_tools_are_checked_against_the_pool() {
    let s = setup();
    let script = "export const meta = {name: 'd', description: 'Deny'}\n\
                  const ok = await agent('a', {disallowedTools: ['mcp__github__*', 'Bash(rm:*)']})\n\
                  let msg = ''\n\
                  try { await agent('b', {disallowedTools: ['bash']}) } catch (e) { msg = e.message }\n\
                  return {ok, msg}";
    s.call(json!({"script": script})).await;
    let note = s.note(0).await;
    let result: Value = serde_json::from_str(field(&note, "result")).expect("result");
    assert_eq!(result["ok"], "echo:a");
    assert_eq!(
        result["msg"],
        "agent() opts.disallowedTools entry 'bash' does not match any tool, but 'Bash' exists \u{2014} tool names are \
         case-sensitive. Refusing the spawn rather than running it with this deny silently dropped."
    );
    let requests = s.fake.requests.lock().expect("lock").clone();
    assert_eq!(requests.len(), 1, "the refused agent never starts");
    assert_eq!(requests[0].deny_tools, ["SendUserMessage", "Agent", "Workflow", "Bash", "Monitor", "mcp__github"]);
    std::fs::remove_dir_all(&s.root).ok();
}

/// Claude Code's throttle backoff: a short reply with no stop reason after
/// more than half the stall window is tried again 45 s later.
#[tokio::test(start_paused = true)]
async fn a_throttled_reply_is_tried_again_after_45_seconds() {
    let s = setup();
    let script = "export const meta = {name: 't', description: 'Throttle'}\n\
                  return await agent('throttle:x', {label: 't', stallMs: 1000})";
    let started = tokio::time::Instant::now();
    s.call(json!({"script": script})).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while s.fake.notes().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let note = s.note(0).await;
    assert_eq!(field(&note, "result"), r#""echo:throttle:x""#);
    assert!(started.elapsed() >= Duration::from_secs(45), "waited {:?}", started.elapsed());
    assert_eq!(s.fake.prompts(), ["throttle:x", "throttle:x"]);
    let session = s.session();
    let records: Vec<_> = std::fs::read_dir(session.join("workflows")).expect("dir").flatten().collect();
    let record = records.iter().find(|e| e.path().extension().is_some_and(|x| x == "json")).expect("record");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).expect("read")).expect("json");
    assert_eq!(
        record["logs"],
        json!(["[t] throttled response (no stop_reason, 3 output tokens in 1s) \u{2014} sleeping 45s before retry"])
    );
    let row = record["workflowProgress"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["type"] == "workflow_agent"))
        .cloned()
        .expect("row");
    assert_eq!(
        (&row["label"], &row["attempt"], &row["lastAttemptReason"]),
        (&json!("t (throttle-retry)"), &json!(2), &json!("throttled"))
    );
    std::fs::remove_dir_all(&s.root).ok();
}

#[tokio::test]
async fn task_stop_stops_a_run_and_resume_replays_it() {
    let s = setup();
    let script = "export const meta = {name: 'chain', description: 'Chain'}\n\
                  const a = await agent('first')\nconst b = await agent('slow: second')\nreturn [a, b]";
    let (launch, _) = s.call(json!({"script": script})).await;
    let id = run_id(&launch);
    let task_id = launch.lines().next().and_then(|l| l.rsplit(' ').next()).expect("task id").to_owned();
    // Wait until the second agent is running.
    for _ in 0..200 {
        if s.fake.prompts().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(s.fake.prompts().len(), 2);
    let task = s.ext.get::<BackgroundTasks>().and_then(|t| t.get(&task_id)).expect("task");
    task.stop().await;
    // A stopped run sends no notification (TaskStop's result says it).
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(s.fake.notes().is_empty());
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(s.session().join(format!("workflows/{id}.json"))).expect("record"),
    )
    .expect("json");
    assert_eq!(record["status"], "killed");
    let second =
        record["workflowProgress"].as_array().expect("rows").iter().find(|r| r["index"] == 2).cloned().expect("row");
    assert_eq!(second["state"], "error");
    assert_eq!(second["endedWithRun"], "stopped");

    // Edited script: the first call replays from the journal, the edited one runs.
    let script_path = s.session().join("workflows/scripts").join(format!("chain-{id}.js"));
    std::fs::write(&script_path, script.replace("slow: second", "second, edited")).expect("edit");
    let (out, err) = s.call(json!({"scriptPath": script_path, "resumeFromRunId": id})).await;
    assert!(!err, "{out}");
    let note = s.note(0).await;
    assert_eq!(field(&note, "result"), r#"["echo:first","echo:second, edited"]"#);
    assert_eq!(s.fake.prompts(), vec!["first", "slow: second", "second, edited"]);
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(s.session().join(format!("workflows/{id}.json"))).expect("record"),
    )
    .expect("json");
    let first =
        record["workflowProgress"].as_array().expect("rows").iter().find(|r| r["index"] == 1).cloned().expect("row");
    assert_eq!(first["cached"], true);
    std::fs::remove_dir_all(&s.root).ok();
}

#[tokio::test]
async fn nested_workflows_share_the_run() {
    let s = setup();
    let saved = s.root.join("home/workflows");
    std::fs::create_dir_all(&saved).expect("dir");
    std::fs::write(
        saved.join("inner.js"),
        "export const meta = {name: 'inner', description: 'Inner'}\nconsole.log('in', args)\nreturn await agent(`inner ${args.n}`)",
    )
    .expect("write");
    let script = "export const meta = {name: 'outer', description: 'Outer'}\n\
                  const a = await workflow('inner', {n: 1})\n\
                  let nested = 'none'\n\
                  try { await workflow('missing') } catch (e) { nested = e.message }\n\
                  return {a, nested}";
    s.call(json!({"script": script})).await;
    let note = s.note(0).await;
    assert_eq!(
        field(&note, "result"),
        r#"{"a":"echo:inner 1","nested":"workflow('missing'): no workflow with that name. Available: inner"}"#,
        "{note}"
    );
    let id = note.split("resumeFromRunId: '").nth(1).and_then(|r| r.split('\'').next()).expect("id").to_owned();
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(s.session().join(format!("workflows/{id}.json"))).expect("record"),
    )
    .expect("json");
    let logs: Vec<&str> = record["logs"].as_array().expect("logs").iter().filter_map(Value::as_str).collect();
    assert_eq!(logs, vec!["\u{25B8} running dynamic workflow inner", "[inner] in {\"n\":1}", "\u{25B8} inner done"]);
    let agent = record["workflowProgress"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["type"] == "workflow_agent")
        .cloned()
        .expect("agent");
    assert_eq!(agent["phaseTitle"], "\u{25B8} inner");
    std::fs::remove_dir_all(&s.root).ok();
}

/// Resume keys are Claude Code's: every scenario of `fixtures/cc-lanes.mjs`
/// (Claude Code's key code under node's AsyncLocalStorage) replays and
/// re-runs the same calls here, under the same keys.
#[tokio::test]
async fn resume_keys_follow_async_lanes_like_claude_code() {
    #[derive(serde::Deserialize)]
    struct Call {
        prompt: String,
        key: String,
        cached: bool,
    }
    #[derive(serde::Deserialize)]
    struct Scenario {
        name: String,
        scripts: Vec<String>,
        runs: Vec<Vec<Call>>,
    }
    let scenarios: Vec<Scenario> = serde_json::from_str(include_str!("fixtures/cc-lanes.json")).expect("fixture");
    for scenario in scenarios {
        let s = setup();
        let mut id = String::new();
        let mut seen = 0;
        for (i, (body, expected)) in scenario.scripts.iter().zip(&scenario.runs).enumerate() {
            let script = format!("export const meta = {{name: 'lanes', description: 'Lanes'}}\n{body}");
            let (out, err) = if i == 0 {
                s.call(json!({"script": script})).await
            } else {
                let path = s.session().join("workflows/scripts").join(format!("lanes-{id}.js"));
                std::fs::write(&path, &script).expect("edit");
                s.call(json!({"scriptPath": path, "resumeFromRunId": id})).await
            };
            assert!(!err, "{}: {out}", scenario.name);
            if i == 0 {
                id = run_id(&out);
            }
            let note = s.note(i).await;
            assert_eq!(field(&note, "status"), "completed", "{}: {note}", scenario.name);

            let journal_path = s.session().join(format!("subagents/workflows/{id}/journal.jsonl"));
            let lines: Vec<Value> = std::fs::read_to_string(&journal_path)
                .expect("journal")
                .lines()
                .map(|l| serde_json::from_str(l).expect("line"))
                .collect();
            let (before, now) = lines.split_at(seen);
            seen = lines.len();
            let mut live: Vec<(String, String)> = now
                .iter()
                .filter(|l| l["type"] == "started")
                .map(|l| {
                    (
                        l["key"].as_str().unwrap_or_default().to_owned(),
                        l["label"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            let record: Value = serde_json::from_str(
                &std::fs::read_to_string(s.session().join(format!("workflows/{id}.json"))).expect("record"),
            )
            .expect("json");
            let mut cached: Vec<String> = record["workflowProgress"]
                .as_array()
                .expect("rows")
                .iter()
                .filter(|r| r["type"] == "workflow_agent" && r["cached"] == true)
                .map(|r| r["promptPreview"].as_str().unwrap_or_default().to_owned())
                .collect();
            let mut want_live: Vec<(String, String)> =
                expected.iter().filter(|c| !c.cached).map(|c| (c.key.clone(), c.prompt.clone())).collect();
            let mut want_cached: Vec<String> = expected.iter().filter(|c| c.cached).map(|c| c.prompt.clone()).collect();
            for call in expected.iter().filter(|c| c.cached) {
                assert!(
                    before.iter().any(|l| l["type"] == "result" && l["key"] == call.key.as_str()),
                    "{} run {i}: {} replays from {}",
                    scenario.name,
                    call.prompt,
                    call.key
                );
            }
            for list in [&mut live, &mut want_live] {
                list.sort();
            }
            cached.sort();
            want_cached.sort();
            assert_eq!(live, want_live, "{} run {i}: live calls", scenario.name);
            assert_eq!(cached, want_cached, "{} run {i}: replayed calls", scenario.name);
        }
        std::fs::remove_dir_all(&s.root).ok();
    }
}

/// `cargo test --release -p eos-agent-workflow --test run hundred_runs -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn hundred_runs_footprint() {
    const RUNS: usize = 100;
    let s = setup();
    // Warm up: one run end to end.
    let one = "export const meta = {name: 'warm', description: 'Warm'}\nreturn await agent('x')";
    s.call(json!({"script": one})).await;
    s.note(0).await;
    // The fake keeps every request and task event; drop them from the numbers.
    let forget = || {
        s.fake.events.lock().expect("lock").clear();
        s.fake.requests.lock().expect("lock").clear();
    };
    forget();
    let before = eos_agent_testkit::footprint_bytes();
    let script = "export const meta = {name: 'fan', description: 'Fan out'}\n\
                  return await parallel(Array.from({length: 10}, (_, i) => () => agent(`wait:300:${i}`, {label: `a${i}`})))";
    let per_run = |b: u64, from: u64| (b.saturating_sub(from)) as f64 / RUNS as f64 / 1024.0;
    let mut notes = 1;
    let mut ends = Vec::new();
    for round in 1..=3 {
        let started = std::time::Instant::now();
        for _ in 0..RUNS {
            let (out, err) = s.call(json!({"script": script})).await;
            assert!(!err, "{out}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let busy = eos_agent_testkit::footprint_bytes();
        notes += RUNS;
        s.note(notes - 1).await;
        let wall = started.elapsed();
        tokio::time::sleep(Duration::from_millis(300)).await;
        forget();
        let after = eos_agent_testkit::footprint_bytes();
        println!(
            "round {round}: {RUNS} runs x 10 agents in {wall:?}; all live {:.1} MB ({:.0} KB per run over the start), \
             after {:.1} MB",
            busy as f64 / 1e6,
            per_run(busy, before),
            after as f64 / 1e6
        );
        ends.push(after);
    }
    println!(
        "rounds 2 and 3 added {:.1} KB and {:.1} KB per run",
        per_run(ends[1], ends[0]),
        per_run(ends[2], ends[1])
    );
    std::fs::remove_dir_all(&s.root).ok();
}

/// Resuming a run that still runs is refused with how to stop it, in the
/// key TaskStop's schema takes.
#[tokio::test]
async fn resuming_a_running_run_names_task_stops_own_key() {
    let s = setup();
    let script = "export const meta = {name: 'wait', description: 'Wait'}\nreturn await agent('slow: wait')";
    let (launch, _) = s.call(json!({"script": script})).await;
    let id = run_id(&launch);
    let task_id = launch.lines().next().and_then(|l| l.rsplit(' ').next()).expect("task id").to_owned();
    let (out, err) = s.call(json!({"script": script, "resumeFromRunId": id})).await;
    assert!(err, "{out}");
    assert!(
        out.contains(&format!("Stop it first with TaskStop({{task_id: \"{task_id}\"}}) before resuming.")),
        "{out}"
    );
    if let Some(task) = s.ext.get::<BackgroundTasks>().and_then(|t| t.get(&task_id)) {
        task.stop().await;
    }
    std::fs::remove_dir_all(&s.root).ok();
}
