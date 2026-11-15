use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eos_agent_core::{
    Agent, AgentConfig, AgentEvent, AllowAll, BoxFuture, CancellationToken, Clock, Deps, EventSink, EventStream,
    ModelClient, ModelError, ModelRequest, Permission, PermissionGate, Reminders, SessionContext, StoredMessage,
    SystemPrompt, Tool, ToolCall, ToolContext, ToolOutput, ToolSource, ToolSpec, Toolbox, TurnOutcome,
};
use eos_agent_types::{ContentBlock, Role, StopReason, StreamEvent, ToolResultContent};
use serde_json::value::RawValue;

// ---- fakes ---------------------------------------------------------------

type Script = Result<Vec<StreamEvent>, ModelError>;

#[derive(Default)]
struct FakeModel {
    script: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl FakeModel {
    fn new(script: impl IntoIterator<Item = Script>) -> Self {
        Self { script: Mutex::new(script.into_iter().collect()), ..Self::default() }
    }
}

impl ModelClient for FakeModel {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        self.requests.lock().expect("lock").push(request.clone());
        let next = self.script.lock().expect("lock").pop_front().expect("script exhausted");
        Box::pin(async move {
            let events = next?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        })
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<AgentEvent>>);

impl EventSink for Collect {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'_, ()> {
        self.0.lock().expect("lock").push(event);
        Box::pin(async {})
    }
}

impl Collect {
    fn count(&self, f: impl Fn(&AgentEvent) -> bool) -> usize {
        self.0.lock().expect("lock").iter().filter(|e| f(e)).count()
    }
}

struct Instant;
impl Clock for Instant {
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Echoes its `text` input after `delay_ms`; concurrency-safe when `safe`.
struct Echo {
    spec: ToolSpec,
    safe: bool,
}

impl Echo {
    fn tool(name: &str, safe: bool) -> Arc<dyn Tool> {
        Arc::new(Self { spec: ToolSpec::new(name, "echo", serde_json::json!({"type": "object"})), safe })
    }
}

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        self.safe
    }
    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let v: serde_json::Value = serde_json::from_str(input.get()).expect("json");
            assert!(v["panic"].as_bool() != Some(true), "echo told to panic");
            let delay = v["delay_ms"].as_u64().unwrap_or(0);
            if v["cancel"].as_bool() == Some(true) {
                ctx.cancel.cancel();
            }
            tokio::time::sleep(Duration::from_millis(delay)).await;
            ToolOutput::text(v["text"].as_str().unwrap_or("").to_owned())
        })
    }
}

struct DenyAll;
impl PermissionGate for DenyAll {
    fn decide<'a>(&'a self, _: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        Box::pin(async { Permission::Deny("not allowed here".into()) })
    }
}

// ---- scripted replies ----------------------------------------------------

fn ev(name: &str, data: &str) -> StreamEvent {
    StreamEvent::parse(name, data).expect("event")
}

fn reply(blocks: &[(&str, &str)], stop: &str) -> Script {
    let mut out = vec![ev(
        "message_start",
        r#"{"message":{"id":"m","model":"x","role":"assistant","usage":{"input_tokens":3}}}"#,
    )];
    for (i, (start, delta)) in blocks.iter().enumerate() {
        out.push(ev("content_block_start", &format!(r#"{{"index":{i},"content_block":{start}}}"#)));
        out.push(ev("content_block_delta", &format!(r#"{{"index":{i},"delta":{delta}}}"#)));
        out.push(ev("content_block_stop", &format!(r#"{{"index":{i}}}"#)));
    }
    out.push(ev("message_delta", &format!(r#"{{"delta":{{"stop_reason":"{stop}"}},"usage":{{"output_tokens":4}}}}"#)));
    out.push(ev("message_stop", "{}"));
    Ok(out)
}

fn text(t: &str) -> Script {
    reply(&[(r#"{"type":"text","text":""}"#, &format!(r#"{{"type":"text_delta","text":"{t}"}}"#))], "end_turn")
}

fn tools(calls: &[(&str, &str, &str)]) -> Script {
    let parts: Vec<(String, String)> = calls
        .iter()
        .map(|(id, name, input)| {
            (
                format!(r#"{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}"#),
                format!(
                    r#"{{"type":"input_json_delta","partial_json":{}}}"#,
                    serde_json::to_string(input).expect("str")
                ),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = parts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    reply(&refs, "tool_use")
}

fn agent(tools: Vec<Arc<dyn Tool>>) -> Agent {
    Agent::new("a1", AgentConfig::new("m"), SystemPrompt::new(&[("sys", None)]), Arc::new(Toolbox::new(tools)))
}

fn user(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: t.into(), citations: None }]
}

fn deps<'a>(model: &'a FakeModel, sink: &'a Collect, gate: &'a dyn PermissionGate) -> Deps<'a> {
    Deps { model, events: sink, permissions: gate, clock: &Instant, store: None }
}

fn tool_results(agent: &Agent, index: usize) -> Vec<(String, bool)> {
    agent.transcript()[index]
        .blocks()
        .expect("blocks")
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content: Some(ToolResultContent::Text(t)), is_error, .. } => Some((t, is_error)),
            _ => None,
        })
        .collect()
}

// ---- tests ---------------------------------------------------------------

#[tokio::test]
async fn plain_reply_ends_the_turn() {
    let model = FakeModel::new([text("hello")]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let out = a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert_eq!(a.transcript().len(), 2);
    assert_eq!(a.transcript()[1].role(), Role::Assistant);
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::TextDelta { text } if text == "hello")), 1);
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::TurnEnded { stated: true, .. })), 1);
    // The newest message carries the cache marker; the stored one does not.
    let req = &model.requests.lock().expect("lock")[0];
    assert!(std::str::from_utf8(&req.messages[0]).expect("utf8").contains("cache_control"));
    assert!(!std::str::from_utf8(a.transcript()[0].json()).expect("utf8").contains("cache_control"));
}

#[tokio::test]
async fn a_reply_without_a_stop_reason_ends_the_turn_unstated() {
    let mut script = text("hi").expect("script");
    let at = script.iter().position(|e| matches!(e, StreamEvent::MessageDelta { .. })).expect("delta");
    script[at] = ev("message_delta", r#"{"delta":{"stop_reason":null},"usage":{"output_tokens":2}}"#);
    let model = FakeModel::new([Ok(script)]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let out = a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    let unstated =
        |e: &AgentEvent| matches!(e, AgentEvent::TurnEnded { stop_reason: StopReason::EndTurn, stated: false });
    assert_eq!(sink.count(unstated), 1);
}

/// Hands out a new toolbox once (an MCP server signed in to mid-turn).
#[derive(Debug)]
struct OneSwap(Mutex<Option<Arc<Toolbox>>>);

impl ToolSource for OneSwap {
    fn update(&self, _current: &Arc<Toolbox>) -> Option<Arc<Toolbox>> {
        self.0.lock().expect("lock").take()
    }
}

#[tokio::test]
async fn a_changed_tool_set_reaches_the_next_request() {
    let model = FakeModel::new([tools(&[("t1", "echo", r#"{"text":"x"}"#)]), text("done")]);
    let sink = Collect::default();
    let mut config = AgentConfig::new("m");
    let swap = Arc::new(OneSwap(Mutex::new(None)));
    config.tool_source = Some(Arc::clone(&swap) as Arc<dyn ToolSource>);
    let mut a = Agent::new("a1", config, SystemPrompt::new(&[("sys", None)]), Arc::new(Toolbox::new(vec![])));
    // The tool set changes before the first request: it carries "echo", and the call runs.
    *swap.0.lock().expect("lock") = Some(Arc::new(Toolbox::new(vec![Echo::tool("echo", false)])));
    a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    let requests = model.requests.lock().expect("lock");
    let names = |r: &ModelRequest| r.tools.as_ref().map(|t| String::from_utf8_lossy(t).contains("\"echo\""));
    assert_eq!(names(&requests[0]), Some(true));
    assert_eq!(tool_results(&a, 2), vec![("x".to_owned(), false)]);
}

#[tokio::test]
async fn tool_round_trip() {
    let model = FakeModel::new([tools(&[("t1", "echo", r#"{"text":"pong"}"#)]), text("done")]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("echo", false)]);
    let out = a.run_turn(user("ping"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert_eq!(a.transcript().len(), 4);
    assert_eq!(tool_results(&a, 2), vec![("pong".to_owned(), false)]);
    // The second request carried the tool result.
    assert_eq!(model.requests.lock().expect("lock")[1].messages.len(), 3);
}

#[tokio::test]
async fn unknown_tool_and_denied_call_return_errors() {
    let model = FakeModel::new([tools(&[("t1", "nope", "{}"), ("t2", "echo", r#"{"text":"x"}"#)]), text("ok")]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("echo", false)]);
    a.run_turn(user("go"), deps(&model, &sink, &DenyAll), &CancellationToken::new()).await.expect("turn");
    let results = tool_results(&a, 2);
    assert!(results[0].0.contains("No such tool") && results[0].1);
    assert_eq!(results[1], ("not allowed here".to_owned(), true));
}

#[tokio::test]
async fn a_panicking_tool_fails_its_own_call_only() {
    let model = FakeModel::new([
        tools(&[("t1", "echo", r#"{"panic":true}"#), ("t2", "echo", r#"{"text":"still here"}"#)]),
        text("ok"),
    ]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("echo", true)]);
    let out = a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    let results = tool_results(&a, 2);
    assert!(results[0].1 && results[0].0.contains("echo told to panic"), "{results:?}");
    assert_eq!(results[1], ("still here".to_owned(), false));
}

#[tokio::test]
async fn parallel_safe_calls_keep_call_order() {
    let model = FakeModel::new([
        tools(&[("t1", "read", r#"{"text":"slow","delay_ms":40}"#), ("t2", "read", r#"{"text":"fast","delay_ms":1}"#)]),
        text("ok"),
    ]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("read", true)]);
    let started = std::time::Instant::now();
    a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(tool_results(&a, 2), vec![("slow".to_owned(), false), ("fast".to_owned(), false)]);
    assert!(started.elapsed() < Duration::from_millis(80), "safe calls should overlap");
}

#[tokio::test]
async fn retries_transient_failures() {
    let model = FakeModel::new([Err(ModelError::Overloaded { retry_after: None }), text("after retry")]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let out = a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::Retrying { .. })), 1);
}

#[tokio::test]
async fn fatal_errors_end_the_turn() {
    let model = FakeModel::new([Err(ModelError::Invalid("bad request".into()))]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let err = a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await;
    assert!(matches!(err, Err(eos_agent_core::TurnError::Model(ModelError::Invalid(_)))));
}

#[tokio::test]
async fn interrupt_during_tools_closes_every_call() {
    let model = FakeModel::new([tools(&[
        ("t1", "echo", r#"{"text":"x","cancel":true,"delay_ms":50}"#),
        ("t2", "echo", r#"{"text":"never"}"#),
    ])]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("echo", false)]);
    let out = a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Interrupted);
    let results = tool_results(&a, 2);
    assert_eq!(results.len(), 2, "every tool_use needs a tool_result");
    assert!(results.iter().all(|(t, err)| *err && t == "Interrupted by user"));
    let last = a.transcript().last().expect("last").blocks().expect("blocks");
    assert!(
        matches!(&last[0], ContentBlock::Text { text, .. } if text == "[Request interrupted by user for tool use]")
    );
}

#[tokio::test]
async fn output_limit_continues_the_turn() {
    let cut = reply(&[(r#"{"type":"text","text":""}"#, r#"{"type":"text_delta","text":"part 1"}"#)], "max_tokens");
    let model = FakeModel::new([cut, text("part 2")]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let out = a.run_turn(user("long"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    let note = a.transcript()[2].blocks().expect("blocks");
    assert!(matches!(&note[0], ContentBlock::Text { text, .. } if text.starts_with("Output token limit hit. Resume")));
}

#[tokio::test]
async fn session_context_rides_the_first_message_only() {
    use eos_agent_core::SessionContext;
    let model = FakeModel::new([text("one"), text("two")]);
    let sink = Collect::default();
    let context = SessionContext {
        user_prefix: vec![ContentBlock::Text {
            text: "<system-reminder>ctx</system-reminder>".into(),
            citations: None,
        }]
        .into(),
        system_notes: vec![eos_agent_core::StoredMessage::system_text("# Environment")],

        ..SessionContext::default()
    };
    let mut a = agent(vec![]).with_session_context(context);
    let cancel = CancellationToken::new();
    a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    a.run_turn(user("again"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    let first = a.transcript()[0].blocks().expect("blocks");
    assert!(matches!(&first[0], ContentBlock::Text { text, .. } if text.contains("ctx")));
    assert!(matches!(&first[1], ContentBlock::Text { text, .. } if text == "hi"));
    assert_eq!(a.transcript()[1].role(), Role::System);
    assert_eq!(a.transcript()[2].role(), Role::Assistant);
    assert_eq!(a.transcript()[3].blocks().expect("blocks").len(), 1, "second turn carries no context");
}

#[tokio::test]
async fn compaction_replaces_history_with_the_summary() {
    use eos_agent_core::{Compaction, CompactionText};
    #[derive(Debug)]
    struct Words;
    impl CompactionText for Words {
        fn instruction(&self, focus: Option<&str>) -> String {
            format!("Summarize.{}", focus.map(|f| format!(" Focus: {f}")).unwrap_or_default())
        }
        fn seed(&self, summary: &str) -> String {
            format!("Continued:\n\n{}", summary.trim())
        }
    }
    let model = FakeModel::new([text("first answer"), text("SUMMARY OF WORK")]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    a.config_mut().compaction = Some(Compaction { threshold_tokens: Some(u64::MAX), text: Arc::new(Words) });
    let cancel = CancellationToken::new();
    a.run_turn(user("do work"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    assert!(a.compact(Some("keep file names"), deps(&model, &sink, &AllowAll), &cancel).await.expect("compact"));

    assert_eq!(a.transcript().len(), 1);
    let seed = a.transcript()[0].blocks().expect("blocks");
    assert!(matches!(&seed[0], ContentBlock::Text { text, .. } if text == "Continued:\n\nSUMMARY OF WORK"));
    let reqs = model.requests.lock().expect("lock");
    let summary_req = &reqs[1];
    let last = std::str::from_utf8(summary_req.messages.last().expect("last")).expect("utf8");
    assert!(last.contains("Summarize.") && last.contains("keep file names"));
    assert!(std::str::from_utf8(summary_req.extra.as_ref().expect("extra")).expect("utf8").contains("tool_choice"));
    // The host hears the summary start (before its request) and end.
    let events = sink.0.lock().expect("lock");
    let at = |f: fn(&AgentEvent) -> bool| events.iter().position(f).expect("event");
    let started = at(|e| matches!(e, AgentEvent::Compacting { .. }));
    let requested = events.iter().rposition(|e| matches!(e, AgentEvent::RequestStarted { .. })).expect("request");
    assert!(started < requested && requested < at(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Compacting { .. })).count(), 1);
}

/// Answers once and ends the turn (Claude Code's StructuredOutput); shows
/// the gate a review of its input.
struct Finisher {
    spec: ToolSpec,
}

impl Tool for Finisher {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn call<'a>(&'a self, _input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async { ToolOutput::text("Structured output provided successfully").ending_turn() })
    }
    fn review<'a>(
        &'a self,
        input: &'a RawValue,
        _ctx: ToolContext<'a>,
    ) -> BoxFuture<'a, Option<eos_agent_core::ToolReview>> {
        Box::pin(async move {
            let filled = format!(r#"{{"seen":{}}}"#, input.get());
            Some(eos_agent_core::ToolReview {
                message: "Review before running".into(),
                input: RawValue::from_string(filled).expect("json").into(),
            })
        })
    }
}

/// Records what it was asked to review.
#[derive(Default)]
struct Reviewing(Mutex<Vec<(String, String)>>);

impl PermissionGate for Reviewing {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        if let Some(review) = call.review {
            self.0.lock().expect("lock").push((review.message.clone(), review.input.get().to_owned()));
        }
        Box::pin(async { Permission::Allow })
    }
}

#[tokio::test]
async fn a_tool_can_end_the_turn_and_show_the_gate_a_review() {
    let model = FakeModel::new([tools(&[("t1", "finish", r#"{"x":1}"#)])]);
    let sink = Collect::default();
    let gate = Reviewing::default();
    let finisher = Arc::new(Finisher { spec: ToolSpec::new("finish", "f", serde_json::json!({"type": "object"})) });
    let mut a = agent(vec![finisher]);
    let out = a.run_turn(user("go"), deps(&model, &sink, &gate), &CancellationToken::new()).await.expect("turn");
    // No second request: the result is recorded and the turn is over.
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert_eq!(model.requests.lock().expect("lock").len(), 1);
    assert_eq!(tool_results(&a, 2), vec![("Structured output provided successfully".to_owned(), false)]);
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::TurnEnded { .. })), 1);
    assert_eq!(
        *gate.0.lock().expect("lock"),
        vec![("Review before running".to_owned(), r#"{"seen":{"x":1}}"#.to_owned())]
    );
}

#[tokio::test]
async fn a_context_overflow_summarizes_once_and_retries() {
    use eos_agent_core::{Compaction, CompactionText};
    #[derive(Debug)]
    struct Words;
    impl CompactionText for Words {
        fn instruction(&self, _: Option<&str>) -> String {
            "Summarize.".into()
        }
        fn seed(&self, summary: &str) -> String {
            format!("Continued: {summary}")
        }
    }
    let too_long = || Err(ModelError::ContextWindowExceeded("too long".into()));
    let model = FakeModel::new([
        text("one"),
        text("two"),
        too_long(),
        // The summary request does not fit either: the older half goes.
        too_long(),
        text("SUMMARY"),
        text("three"),
    ]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    a.config_mut().compaction = Some(Compaction { threshold_tokens: Some(u64::MAX), text: Arc::new(Words) });
    let cancel = CancellationToken::new();
    for prompt in ["a", "b"] {
        a.run_turn(user(prompt), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    }
    let outcome = a.run_turn(user("c"), deps(&model, &sink, &AllowAll), &cancel).await.expect("recovered");
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));
    let reqs = model.requests.lock().expect("lock");
    assert_eq!(reqs[3].messages.len(), 6, "the whole history and the instruction");
    assert_eq!(reqs[4].messages.len(), 4, "the later half (from a user prompt) and the instruction");
    assert_eq!(a.transcript()[0].text(), "Continued: SUMMARY");
    assert_eq!(a.transcript().last().map(StoredMessage::text).as_deref(), Some("three"));
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::Compacted { .. })), 1);
}

#[tokio::test]
async fn max_turns_ends_the_turn_after_the_tool_results() {
    let model =
        FakeModel::new([tools(&[("t1", "echo", r#"{"text":"a"}"#)]), tools(&[("t2", "echo", r#"{"text":"b"}"#)])]);
    let sink = Collect::default();
    let mut a = agent(vec![Echo::tool("echo", true)]);
    a.config_mut().max_turns = Some(2);
    let out = a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::Other("max_turns".into())));
    assert_eq!(model.requests.lock().expect("lock").len(), 2, "two requests, as Claude Code's maxTurns: 2");
    assert_eq!(a.transcript().last().map(StoredMessage::role), Some(Role::User), "the last results are kept");
}

/// A deferred tool that appears mid-turn is announced in a system note, as
/// Claude Code's `deferred_tools_delta`.
#[tokio::test]
async fn new_deferred_tools_are_announced() {
    let deferred = |name: &str| -> Arc<dyn Tool> {
        Arc::new(Echo {
            spec: ToolSpec::new(name, "echo", serde_json::json!({"type": "object"})).deferred(),
            safe: true,
        })
    };
    let model = FakeModel::new([tools(&[("t1", "echo", r#"{"text":"x"}"#)]), text("done")]);
    let sink = Collect::default();
    let mut config = AgentConfig::new("m");
    let swap = Arc::new(OneSwap(Mutex::new(None)));
    config.tool_source = Some(Arc::clone(&swap) as Arc<dyn ToolSource>);
    let first = Toolbox::new(vec![Echo::tool("echo", false), Echo::tool("ToolSearch", true)]);
    let mut a = Agent::new("a1", config, SystemPrompt::new(&[("sys", None)]), Arc::new(first));
    // After the first turn, an MCP server's tool arrives.
    let later =
        Toolbox::new(vec![Echo::tool("echo", false), Echo::tool("ToolSearch", true), deferred("mcp__gh__issue")]);
    a.run_turn(user("go"), deps(&model, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    *swap.0.lock().expect("lock") = Some(Arc::new(later));
    let model2 = FakeModel::new([text("ok")]);
    a.run_turn(user("again"), deps(&model2, &sink, &AllowAll), &CancellationToken::new()).await.expect("turn");
    let note = a.transcript().iter().rev().find(|m| m.role() == Role::System).expect("a system note").text();
    assert!(note.starts_with("The following deferred tools are now available via ToolSearch."), "{note}");
    assert!(note.ends_with("\nmcp__gh__issue"));
}

/// Rewrites every call's input and notes who asked.
#[derive(Default)]
struct Rewriting(Mutex<Vec<(String, bool, Option<String>)>>);

impl PermissionGate for Rewriting {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let parent = call.origin.map(|o| o.parent_id.to_string());
        self.0.lock().expect("lock").push((call.root_id().to_owned(), call.in_workflow(), parent));
        let input = RawValue::from_string(r#"{"text":"as the host put it"}"#.to_owned()).expect("json");
        Box::pin(async move { Permission::AllowWith(Arc::from(input)) })
    }
}

#[tokio::test]
async fn the_gate_can_change_a_calls_input_and_sees_who_started_the_agent() {
    let script = || [tools(&[("t1", "echo", r#"{"text":"as the model put it"}"#)]), text("done")];
    let sink = Collect::default();
    let gate = Rewriting::default();

    let model = FakeModel::new(script());
    let mut top = agent(vec![Echo::tool("echo", false)]);
    top.run_turn(user("go"), deps(&model, &sink, &gate), &CancellationToken::new()).await.expect("turn");
    assert_eq!(tool_results(&top, 2), vec![("as the host put it".to_owned(), false)]);
    // The conversation keeps the call as the model wrote it.
    assert!(top.transcript()[1].text().is_empty());
    let recorded = std::str::from_utf8(top.transcript()[1].json()).expect("utf8");
    assert!(recorded.contains("as the model put it"), "{recorded}");

    let model = FakeModel::new(script());
    let extensions = eos_agent_core::Extensions::default();
    extensions.insert(eos_agent_core::AgentOrigin {
        root_id: "root".into(),
        parent_id: "parent".into(),
        tool_use_id: Some("toolu_agent".into()),
        workflow: Some(eos_agent_core::WorkflowTag { run_id: "r1".into(), agent_index: 1 }),
        modes: Default::default(),
    });
    let mut sub = agent(vec![Echo::tool("echo", false)]).with_extensions(extensions);
    sub.run_turn(user("go"), deps(&model, &sink, &gate), &CancellationToken::new()).await.expect("turn");

    let asked = gate.0.lock().expect("lock").clone();
    assert_eq!(asked[0], ("a1".to_owned(), false, None), "a top-level agent is its own root");
    assert_eq!(asked[1], ("root".to_owned(), true, Some("parent".to_owned())));
}

/// With no threshold, summarizing is the host's call alone: a request that
/// does not fit fails the turn, and the host can still ask for a summary.
#[tokio::test]
async fn without_a_threshold_the_agent_never_summarizes_on_its_own() {
    use eos_agent_core::{Compaction, CompactionText};
    #[derive(Debug)]
    struct Words;
    impl CompactionText for Words {
        fn instruction(&self, _: Option<&str>) -> String {
            "Summarize.".into()
        }
        fn seed(&self, summary: &str) -> String {
            format!("Continued: {summary}")
        }
    }
    let model = FakeModel::new([
        text("one"),
        Err(ModelError::ContextWindowExceeded("too long".into())),
        text("SUMMARY"),
        text("three"),
    ]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    a.config_mut().compaction = Some(Compaction { threshold_tokens: None, text: Arc::new(Words) });
    let cancel = CancellationToken::new();
    a.run_turn(user("a"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    let failed = a.run_turn(user("b"), deps(&model, &sink, &AllowAll), &cancel).await.expect_err("too long");
    assert!(matches!(failed, eos_agent_core::TurnError::Model(ModelError::ContextWindowExceeded(_))));
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::Compacted { .. })), 0);

    assert!(a.compact(None, deps(&model, &sink, &AllowAll), &cancel).await.expect("summary"));
    assert_eq!(a.transcript()[0].text(), "Continued: SUMMARY");
    a.run_turn(user("c"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
}

fn roles(request: &ModelRequest) -> Vec<String> {
    let role = |m: &[u8]| serde_json::from_slice::<serde_json::Value>(m).expect("json")["role"].to_string();
    request.messages.iter().map(|m| role(m).trim_matches('"').to_owned()).collect()
}

/// Where the API refuses a request's `role: "system"` turns: one may not
/// open the conversation, and a run of them must come right before the
/// model's reply or end the array ("role 'system' must precede an
/// 'assistant' message or end the array").
fn misplaced_system_turn(request: &ModelRequest) -> Option<usize> {
    let roles = roles(request);
    let system = |i: usize| roles[i] == "system";
    (0..roles.len()).find(|&i| {
        system(i) && (i == 0 || roles.get(i + 1).is_some_and(|next| next != "system" && next != "assistant"))
    })
}

fn with_environment_note(agent: Agent) -> Agent {
    agent.with_session_context(SessionContext {
        user_prefix: vec![ContentBlock::Text {
            text: "<system-reminder>ctx</system-reminder>".into(),
            citations: None,
        }]
        .into(),
        system_notes: vec![StoredMessage::system_text("# Environment")],
        ..SessionContext::default()
    })
}

/// The audit's chat: summarized by its host (rewound to nothing, the
/// summary left as a note for the next request), rewound to its first
/// message, then a file it had read changed before the next reply. Every
/// request keeps its system turn right before the reply.
#[tokio::test]
async fn system_turns_stay_right_before_the_reply() {
    let model = FakeModel::new([text("one"), text("two"), text("three")]);
    let sink = Collect::default();
    let mut a = with_environment_note(agent(vec![]));
    let cancel = CancellationToken::new();
    a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    a.truncate(0, None).await.expect("rewound");
    a.extensions().get_or_default::<Reminders>().push("This session is being continued from a summary.");
    a.run_turn(user("go on"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    a.truncate(0, None).await.expect("rewound");
    a.extensions().get_or_default::<Reminders>().push("<system-reminder>a.py was modified</system-reminder>");
    a.run_turn(user("again"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let requests = model.requests.lock().expect("lock");
    for request in requests.iter() {
        assert_eq!(misplaced_system_turn(request), None, "{:?}", roles(request));
    }
    assert_eq!(roles(&requests[2]), ["user", "user", "system"]);
    let stored: Vec<Role> = a.transcript().iter().map(StoredMessage::role).collect();
    assert_eq!(stored, [Role::User, Role::User, Role::System, Role::Assistant], "recorded as sent");
    let first = a.transcript()[0].blocks().expect("blocks");
    assert!(
        matches!(&first[0], ContentBlock::Text { text, .. } if text.contains("ctx")),
        "context leads the first message"
    );
}

/// A chat recorded before that order held (user, system note, the next
/// note and messages after it) goes out valid the next time it is used.
#[tokio::test]
async fn a_history_with_a_misplaced_system_turn_is_sent_valid() {
    let model = FakeModel::new([text("back")]);
    let sink = Collect::default();
    let damaged = vec![
        StoredMessage::new(Role::User, &user("hi")).expect("message"),
        StoredMessage::system_text("# Environment"),
        StoredMessage::user_text("<system-reminder>a.py was modified</system-reminder>"),
        StoredMessage::user_text("hello?"),
    ];
    let mut a = agent(vec![]).with_history(damaged.clone());
    let cancel = CancellationToken::new();
    a.run_turn(user("still there?"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let requests = model.requests.lock().expect("lock");
    assert_eq!(roles(&requests[0]), ["user", "user", "user", "user", "system"]);
    assert_eq!(misplaced_system_turn(&requests[0]), None);
    let stored: Vec<Role> = a.transcript().iter().map(StoredMessage::role).collect();
    assert_eq!(&stored[..4], damaged.iter().map(StoredMessage::role).collect::<Vec<_>>(), "the record stays as it was");
}

/// A system note and a queued user note for the same request (a tool set
/// that changed, a file that changed): the user's note first.
#[tokio::test]
async fn a_queued_note_goes_before_a_tool_change_note() {
    let model = FakeModel::new([text("one"), text("two")]);
    let sink = Collect::default();
    let mut a = agent(vec![]);
    let cancel = CancellationToken::new();
    a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    let later =
        Echo { spec: ToolSpec::new("Later", "echo", serde_json::json!({"type": "object"})).deferred(), safe: true };
    a.set_toolbox(Arc::new(Toolbox::new(vec![Arc::new(later)])));
    a.extensions().get_or_default::<Reminders>().push("<system-reminder>a.py was modified</system-reminder>");
    a.run_turn(user("next"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let requests = model.requests.lock().expect("lock");
    assert_eq!(roles(&requests[1]), ["user", "assistant", "user", "user", "system"]);
}

fn message_texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .map(|m| {
            let message: serde_json::Value = serde_json::from_slice(m).expect("json");
            let blocks = message["content"].as_array().cloned().unwrap_or_default();
            blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("|")
        })
        .collect()
}

/// The audit's sequence with a host that summarizes on its own (Claude
/// Code's seed, pushed with `shouldQuery: false`): a turn, the summary in
/// place of the conversation, the next message, a rewind to that message,
/// and the message after it. The summary stays ahead of the user's
/// message through the rewind, the session context comes with the message
/// that opens the conversation, every request keeps its system turn right
/// before the reply, and each request starts with the one before it.
#[tokio::test]
async fn a_hosts_summary_comes_before_the_next_message_and_survives_a_rewind_to_it() {
    let model = FakeModel::new([text("one"), text("two"), text("three"), text("four")]);
    let sink = Collect::default();
    let later =
        Echo { spec: ToolSpec::new("Later", "echo", serde_json::json!({"type": "object"})).deferred(), safe: true };
    let mut a = with_environment_note(agent(vec![Arc::new(later)]));
    let cancel = CancellationToken::new();
    a.run_turn(user("read a.py"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    a.replace_context(user("SUMMARY: a.py was read"), deps(&model, &sink, &AllowAll)).await.expect("replaced");
    assert!(a.transcript()[0].is_meta());
    assert!(a.resume_turn(deps(&model, &sink, &AllowAll), &cancel).await.expect("resume").is_none(), "no turn ran");
    a.run_turn(user("go on"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    a.run_turn(user("and more"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let go_on = a.transcript().iter().position(|m| m.text().ends_with("go on")).expect("the message");
    assert_eq!(go_on, 1, "the summary comes first");
    a.truncate(go_on, None).await.expect("rewound");
    a.run_turn(user("again"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let requests = model.requests.lock().expect("lock");
    for request in requests.iter() {
        assert_eq!(misplaced_system_turn(request), None, "{:?}", roles(request));
    }
    let wire = |i: usize| -> Vec<&[u8]> { requests[i].messages.iter().map(|m| &m[..]).collect() };
    let (opened, next) = (wire(1), wire(2));
    assert_eq!(&next[..opened.len() - 1], &opened[..opened.len() - 1], "the cache prefix holds turn to turn");

    // The session's notes already list the deferred tools: no second note.
    let after = &requests[3];
    assert_eq!(roles(after), ["user", "user", "system"]);
    assert_eq!(
        message_texts(after),
        ["SUMMARY: a.py was read", "<system-reminder>ctx</system-reminder>|again", "# Environment"]
    );
}

/// A summary the agent makes on its own opens the conversation again as a
/// first message does: the session context leads it, and the system notes
/// come right before the request that follows.
#[tokio::test]
async fn a_summary_keeps_the_session_context() {
    use eos_agent_core::{Compaction, CompactionText};
    #[derive(Debug)]
    struct Words;
    impl CompactionText for Words {
        fn instruction(&self, _: Option<&str>) -> String {
            "Summarize.".into()
        }
        fn seed(&self, summary: &str) -> String {
            format!("Continued: {summary}")
        }
    }
    let model = FakeModel::new([text("one"), text("SUMMARY"), text("two"), text("three")]);
    let sink = Collect::default();
    let mut a = with_environment_note(agent(vec![]));
    let cancel = CancellationToken::new();
    a.run_turn(user("hi"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    // Past the threshold from here on: the next request is preceded by a summary.
    a.config_mut().compaction = Some(Compaction { threshold_tokens: Some(1), text: Arc::new(Words) });
    a.run_turn(user("go on"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");
    assert_eq!(sink.count(|e| matches!(e, AgentEvent::Compacted { .. })), 1);
    a.config_mut().compaction = Some(Compaction { threshold_tokens: None, text: Arc::new(Words) });
    a.run_turn(user("again"), deps(&model, &sink, &AllowAll), &cancel).await.expect("turn");

    let requests = model.requests.lock().expect("lock");
    for request in requests.iter() {
        assert_eq!(misplaced_system_turn(request), None, "{:?}", roles(request));
    }
    assert_eq!(roles(&requests[2]), ["user", "system"], "the summary, then the notes");
    let opening = std::str::from_utf8(&requests[2].messages[0]).expect("utf8");
    assert!(opening.contains("ctx") && opening.contains("Continued: SUMMARY"), "{opening}");
    assert_eq!(roles(&requests[3]), ["user", "system", "assistant", "user"], "the context rides once");
    let first = a.transcript()[0].blocks().expect("blocks");
    assert!(matches!(&first[0], ContentBlock::Text { text, .. } if text.contains("ctx")));
}
