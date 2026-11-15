//! Every request keeps each tool call paired with its result, judged by the
//! mock API's copy of the API's rule: for the sequences that broke it
//! (a response whose calls came with a stop reason other than `tool_use`,
//! then another message), for histories damaged by them or by a quit, and
//! for random interleavings of everything that changes a conversation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eos_agent_core::{
    Agent, AgentConfig, AgentEvent, BoxFuture, CancellationToken, Clock, Compaction, CompactionText, Deps, EventSink,
    EventStream, ModelClient, ModelError, ModelRequest, Permission, PermissionGate, Reminders, StoredMessage,
    SystemPrompt, Tool, ToolCall, ToolContext, ToolOutput, ToolSpec, Toolbox, TurnOutcome,
};
use eos_agent_types::{ContentBlock, Role, StopReason, StreamEvent, ToolResultContent};
use serde_json::value::RawValue;

// ---- the judge -------------------------------------------------------------

/// What the API would refuse this request for.
fn violation(request: &ModelRequest) -> Option<String> {
    let mut body = b"{\"messages\":[".to_vec();
    for (i, m) in request.messages.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(m);
    }
    body.extend_from_slice(b"]}");
    eos_agent_mock_api::pairing_violation(&body)
}

// ---- fakes -----------------------------------------------------------------

type Script = Result<Vec<StreamEvent>, ModelError>;

/// Answers from a script, then (when it runs out) from `random`; records
/// every request and what the judge says of it.
#[derive(Default)]
struct Model {
    script: Mutex<VecDeque<Script>>,
    random: Option<Mutex<fastrand::Rng>>,
    requests: Mutex<Vec<ModelRequest>>,
    refused: Mutex<Vec<String>>,
    /// The running turn's token, for a reply that interrupts its stream.
    cancel: Mutex<Option<CancellationToken>>,
}

impl Model {
    fn scripted(script: impl IntoIterator<Item = Script>) -> Self {
        Self { script: Mutex::new(script.into_iter().collect()), ..Self::default() }
    }

    fn refused(&self) -> Vec<String> {
        self.refused.lock().expect("lock").clone()
    }
}

impl ModelClient for Model {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        self.requests.lock().expect("lock").push(request.clone());
        if let Some(why) = violation(request) {
            self.refused.lock().expect("lock").push(why.clone());
            return Box::pin(async move { Err(ModelError::Invalid(why)) });
        }
        let summary = request.extra.as_ref().is_some_and(|e| e.windows(11).any(|w| w == b"tool_choice"));
        let next = self.script.lock().expect("lock").pop_front();
        let (events, hang) = match (next, &self.random) {
            (Some(script), _) => (script, false),
            (None, Some(_)) if summary => (text("summary"), false),
            (None, Some(rng)) => random_reply(&mut rng.lock().expect("lock"), &self.cancel),
            (None, None) => panic!("script exhausted"),
        };
        Box::pin(async move {
            let items: Vec<Result<StreamEvent, ModelError>> = match events {
                Ok(events) => events.into_iter().map(Ok).collect(),
                Err(e) => return Err(e),
            };
            let stream = futures_util::stream::iter(items);
            let stream: EventStream = if hang {
                Box::pin(futures_util::StreamExt::chain(stream, futures_util::stream::pending()))
            } else {
                Box::pin(stream)
            };
            Ok(stream)
        })
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<AgentEvent>>);

impl EventSink for Sink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'_, ()> {
        self.0.lock().expect("lock").push(event);
        Box::pin(async {})
    }
}

struct Now;
impl Clock for Now {
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Denies the tool named `denied`, allows the rest.
struct Gate;
impl PermissionGate for Gate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let denied = call.name == "denied";
        Box::pin(async move { if denied { Permission::Deny("not allowed".into()) } else { Permission::Allow } })
    }
}

/// `echo` answers, `fail` fails, `cancel` interrupts the turn, `hang` never
/// returns (its turn is dropped, as a quit drops it), `denied` is refused by
/// the gate; `write` records that it ran.
struct Kit {
    spec: ToolSpec,
    ran: Arc<Mutex<Vec<String>>>,
}

impl Tool for Kit {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn is_concurrency_safe(&self, _: &RawValue) -> bool {
        self.spec.name() == "echo"
    }
    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            self.ran.lock().expect("lock").push(format!("{}:{}", self.spec.name(), input.get()));
            match self.spec.name() {
                "fail" => ToolOutput::error("it failed"),
                "cancel" => {
                    ctx.cancel.cancel();
                    ToolOutput::text("cancelled")
                }
                "hang" => std::future::pending().await,
                "empty" => ToolOutput::blocks(Vec::new()),
                _ => ToolOutput::text("done"),
            }
        })
    }
}

fn toolbox(ran: &Arc<Mutex<Vec<String>>>) -> Arc<Toolbox> {
    let tools: Vec<Arc<dyn Tool>> = ["echo", "write", "fail", "cancel", "hang", "denied", "empty"]
        .into_iter()
        .map(|name| {
            Arc::new(Kit {
                spec: ToolSpec::new(name, "t", serde_json::json!({"type": "object"})),
                ran: Arc::clone(ran),
            }) as Arc<dyn Tool>
        })
        .collect();
    Arc::new(Toolbox::new(tools))
}

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

fn agent(ran: &Arc<Mutex<Vec<String>>>, history: Vec<StoredMessage>) -> Agent {
    let mut config = AgentConfig::new("m");
    config.compaction = Some(Compaction { threshold_tokens: None, text: Arc::new(Words) });
    config.max_continuations = 2;
    config.retry.max_retries = 1;
    Agent::new("a1", config, SystemPrompt::new(&[("sys", None)]), toolbox(ran)).with_history(history)
}

fn deps<'a>(model: &'a Model, sink: &'a Sink) -> Deps<'a> {
    Deps { model, events: sink, permissions: &Gate, clock: &Now, store: None }
}

// ---- replies ---------------------------------------------------------------

fn ev(name: &str, data: &str) -> StreamEvent {
    StreamEvent::parse(name, data).expect("event")
}

enum Block<'a> {
    Thinking,
    Text(&'a str),
    /// id, name, input JSON; `cut`: the input stops half way and the block never closes.
    Call(&'a str, &'a str, &'a str, bool),
}

fn reply(blocks: &[Block<'_>], stop: Option<&str>) -> Script {
    let mut out = vec![ev(
        "message_start",
        r#"{"message":{"id":"m","model":"m","role":"assistant","usage":{"input_tokens":3}}}"#,
    )];
    for (i, block) in blocks.iter().enumerate() {
        let (start, delta, close) = match block {
            Block::Thinking => (
                r#"{"type":"thinking","thinking":"","signature":""}"#.to_owned(),
                r#"{"type":"signature_delta","signature":"sig"}"#.to_owned(),
                true,
            ),
            Block::Text(t) => (
                r#"{"type":"text","text":""}"#.to_owned(),
                format!(r#"{{"type":"text_delta","text":{}}}"#, serde_json::to_string(t).expect("str")),
                true,
            ),
            Block::Call(id, name, input, cut) => {
                let input = if *cut { &input[..input.len() / 2] } else { input };
                (
                    format!(r#"{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}"#),
                    format!(
                        r#"{{"type":"input_json_delta","partial_json":{}}}"#,
                        serde_json::to_string(input).expect("str")
                    ),
                    !cut,
                )
            }
        };
        out.push(ev("content_block_start", &format!(r#"{{"index":{i},"content_block":{start}}}"#)));
        out.push(ev("content_block_delta", &format!(r#"{{"index":{i},"delta":{delta}}}"#)));
        if close {
            out.push(ev("content_block_stop", &format!(r#"{{"index":{i}}}"#)));
        }
    }
    let stop = stop.map_or("null".to_owned(), |s| format!("\"{s}\""));
    out.push(ev("message_delta", &format!(r#"{{"delta":{{"stop_reason":{stop}}},"usage":{{"output_tokens":4}}}}"#)));
    out.push(ev("message_stop", "{}"));
    Ok(out)
}

fn text(t: &str) -> Script {
    reply(&[Block::Text(t)], Some("end_turn"))
}

const TOOLS: [&str; 8] = ["echo", "write", "fail", "cancel", "denied", "nope", "empty", "echo"];
const STOPS: [Option<&str>; 7] = [
    Some("tool_use"),
    Some("end_turn"),
    Some("max_tokens"),
    Some("stop_sequence"),
    Some("refusal"),
    None,
    Some("tool_use"),
];

/// A random response: text, calls under any stop reason, a call cut off
/// mid-input, a response the stream breaks off (retried), or one the user
/// interrupts while it streams.
fn random_reply(rng: &mut fastrand::Rng, cancel: &Mutex<Option<CancellationToken>>) -> (Script, bool) {
    let ids: Vec<String> = (0..3).map(|_| format!("toolu_{:016x}", rng.u64(..))).collect();
    let n = rng.usize(1..=3);
    let names: Vec<&str> = (0..n).map(|_| TOOLS[rng.usize(..TOOLS.len())]).collect();
    let mut blocks = vec![];
    if rng.bool() {
        blocks.push(Block::Thinking);
    }
    match rng.u8(..10) {
        0 | 1 => return (text("ok"), false),
        2 => {
            blocks.push(Block::Text("cut"));
            return (reply(&blocks, Some("max_tokens")), false);
        }
        3 => {
            // Cut mid-input at the output limit.
            blocks.push(Block::Call(&ids[0], "write", r#"{"file_path":"/x","content":"long text"}"#, true));
            return (reply(&blocks, Some("max_tokens")), false);
        }
        4 => {
            // The connection breaks after the calls streamed: the request is retried.
            for (id, name) in ids.iter().zip(&names) {
                blocks.push(Block::Call(id, name, r#"{"a":1}"#, false));
            }
            let mut events = reply(&blocks, Some("tool_use")).expect("events");
            events.truncate(events.len() - 2);
            return (Ok(events), false);
        }
        5 => {
            // The user interrupts while the calls stream.
            for (id, name) in ids.iter().zip(&names) {
                blocks.push(Block::Call(id, name, r#"{"a":1}"#, false));
            }
            let mut events = reply(&blocks, None).expect("events");
            events.truncate(events.len() - 2);
            if let Some(token) = cancel.lock().expect("lock").as_ref() {
                token.cancel();
            }
            return (Ok(events), true);
        }
        _ => {}
    }
    for (id, name) in ids.iter().zip(&names) {
        blocks.push(Block::Call(id, name, r#"{"a":1}"#, false));
    }
    (reply(&blocks, STOPS[rng.usize(..STOPS.len())]), false)
}

fn blocks_of(message: &StoredMessage) -> Vec<ContentBlock> {
    message.blocks().expect("blocks")
}

fn call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolUse { id: id.into(), name: name.into(), input: RawValue::from_string("{}".into()).expect("json") }
}

fn user(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: text.into(), citations: None }]
}

// ---- the sequence that broke ---------------------------------------------

/// A response that calls a tool but says `end_turn` (or no stop reason at
/// all): the call runs and the turn goes on, so a message that arrives later
/// (another agent's, to a subagent that finished) never lands between the
/// call and its result.
#[tokio::test]
async fn a_call_under_any_stop_reason_is_run_and_answered() {
    for stop in [Some("end_turn"), None, Some("stop_sequence"), Some("refusal"), Some("max_tokens")] {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let write = reply(&[Block::Thinking, Block::Call("toolu_w", "write", r#"{"file_path":"/p"}"#, false)], stop);
        let model = Model::scripted([write, text("written"), text("here is the summary")]);
        let sink = Sink::default();
        let mut a = agent(&ran, Vec::new());
        let cancel = CancellationToken::new();
        let out = a.run_turn(user("plan it"), deps(&model, &sink), &cancel).await.expect("turn");
        assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn), "{stop:?}");
        assert_eq!(ran.lock().expect("lock").len(), 1, "{stop:?}: the call ran");
        let results = blocks_of(&a.transcript()[2]);
        assert!(
            matches!(&results[0], ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "toolu_w")
        );
        // The other agent's message, as SendMessage resumes a subagent with it.
        a.run_turn(user("where is the file?"), deps(&model, &sink), &cancel).await.expect("next turn");
        assert_eq!(model.refused(), Vec::<String>::new(), "{stop:?}");
    }
}

/// The conversation as an older engine stored it: a call, the other agent's
/// message, nothing answering the call. Its next turn records the call as
/// cut off and sends a valid request.
#[tokio::test]
async fn a_conversation_stored_with_an_unanswered_call_works_again() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let history = vec![
        StoredMessage::user_text("plan it"),
        StoredMessage::new(Role::Assistant, &[call("toolu_w", "write")]).expect("message"),
        StoredMessage::user_text("Your run ended but the file does not exist"),
    ];
    let model = Model::scripted([text("I will write it again")]);
    let sink = Sink::default();
    let mut a = agent(&ran, history);
    a.run_turn(user("go on"), deps(&model, &sink), &CancellationToken::new()).await.expect("turn");
    assert_eq!(model.refused(), Vec::<String>::new());
    // What the store gets: after what was there, the call answered as cut
    // off, ahead of the new input in the same message.
    let settled = blocks_of(&a.transcript()[3]);
    assert!(
        matches!(&settled[..], [ContentBlock::ToolResult { tool_use_id, is_error: true, content: Some(ToolResultContent::Text(t)) }, ContentBlock::Text { .. }]
        if tool_use_id == "toolu_w" && t.starts_with("This call was cut off"))
    );
    // What the model got: the result right after the call, the message after it.
    let sent = &model.requests.lock().expect("lock")[0];
    let next: serde_json::Value = serde_json::from_slice(&sent.messages[2]).expect("json");
    assert_eq!(next["content"][0]["tool_use_id"], "toolu_w");
    assert_eq!(next["content"][1]["text"], "Your run ended but the file does not exist");
    assert!(ran.lock().expect("lock").is_empty(), "a cut-off call is not run again");
}

/// The app quit while tools ran: the transcript ends with the model's calls.
/// The host's resume carries the turn on with them answered as cut off.
#[tokio::test]
async fn a_turn_cut_off_in_its_tools_resumes_with_them_answered() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let history = vec![
        StoredMessage::user_text("do two things"),
        StoredMessage::new(Role::Assistant, &[call("t1", "echo"), call("t2", "write")]).expect("message"),
    ];
    let model = Model::scripted([text("carrying on")]);
    let sink = Sink::default();
    let mut a = agent(&ran, history);
    let out = a.resume_turn(deps(&model, &sink), &CancellationToken::new()).await.expect("resume");
    assert_eq!(out, Some(TurnOutcome::Finished(StopReason::EndTurn)));
    assert_eq!(model.refused(), Vec::<String>::new());
    let answered: Vec<String> = blocks_of(&a.transcript()[2])
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect();
    assert_eq!(answered, ["t1", "t2"]);
    let heard = sink.0.lock().expect("lock").iter().filter(|e| matches!(e, AgentEvent::ToolResult { .. })).count();
    assert_eq!(heard, 2, "the host hears each call end");
}

/// A call whose input the output limit cut off is answered without running.
#[tokio::test]
async fn a_call_cut_off_mid_input_is_answered_not_run() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let cut = reply(
        &[Block::Call("toolu_c", "write", r#"{"file_path":"/p","content":"a long body"}"#, true)],
        Some("max_tokens"),
    );
    let model = Model::scripted([cut, text("smaller then")]);
    let sink = Sink::default();
    let mut a = agent(&ran, Vec::new());
    a.run_turn(user("write"), deps(&model, &sink), &CancellationToken::new()).await.expect("turn");
    assert!(ran.lock().expect("lock").is_empty());
    let result = blocks_of(&a.transcript()[2]);
    assert!(
        matches!(&result[0], ContentBlock::ToolResult { is_error: true, content: Some(ToolResultContent::Text(t)), .. }
        if t.contains("cut off before it was complete"))
    );
    assert_eq!(model.refused(), Vec::<String>::new());
}

/// Results with no call before them, a duplicated call, a result put after
/// a note: sent paired, whatever the record holds.
#[tokio::test]
async fn any_stored_damage_goes_out_paired() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let result = |id: &str| ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: Some(ToolResultContent::Text("r".into())),
        is_error: false,
    };
    let history = vec![
        StoredMessage::new(Role::User, &[result("gone")]).expect("m"),
        StoredMessage::user_text("hi"),
        StoredMessage::new(Role::Assistant, &[call("t1", "echo")]).expect("m"),
        StoredMessage::system_text("a note"),
        StoredMessage::user_text("a stray message"),
        StoredMessage::new(Role::User, &[result("t1"), result("t9")]).expect("m"),
        StoredMessage::new(Role::Assistant, &[call("t1", "echo"), call("t2", "echo")]).expect("m"),
        StoredMessage::new(Role::User, &[result("t2"), result("t2")]).expect("m"),
        StoredMessage::new(Role::Assistant, &[ContentBlock::Text { text: "ok".into(), citations: None }]).expect("m"),
        StoredMessage::new(Role::User, &[result("t2")]).expect("m"),
        StoredMessage::new(Role::Assistant, &[ContentBlock::Text { text: "and".into(), citations: None }]).expect("m"),
    ];
    let model = Model::scripted([text("fine")]);
    let sink = Sink::default();
    let mut a = agent(&ran, history);
    a.run_turn(user("next"), deps(&model, &sink), &CancellationToken::new()).await.expect("turn");
    assert_eq!(model.refused(), Vec::<String>::new());
}

// ---- random interleavings --------------------------------------------------

/// Random turns, notes, interrupts, quits mid-tools, resumes from a cut
/// transcript, rewinds, summaries, host seeds and stored damage: no request
/// ever breaks the pairing rule, and the record each step leaves pairs
/// whenever the step did not damage it on purpose.
#[tokio::test]
async fn random_interleavings_never_send_an_unpaired_request() {
    for seed in 0..150u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let ran = Arc::new(Mutex::new(Vec::new()));
        let model = Model { random: Some(Mutex::new(fastrand::Rng::with_seed(seed ^ 0x5eed))), ..Model::default() };
        let sink = Sink::default();
        let mut a = agent(&ran, Vec::new());
        for step in 0..14 {
            let cancel = CancellationToken::new();
            *model.cancel.lock().expect("lock") = Some(cancel.clone());
            let d = deps(&model, &sink);
            match rng.u8(..12) {
                0..=3 => drop(a.run_turn(user("go"), d, &cancel).await),
                4 => {
                    a.extensions().get_or_default::<Reminders>().push("<system-reminder>a note</system-reminder>");
                    drop(a.run_turn(user("and"), d, &cancel).await);
                }
                5 => drop(a.resume_turn(d, &cancel).await),
                6 => {
                    // A quit while a tool runs: the turn's future is dropped.
                    model.script.lock().expect("lock").push_back(reply(
                        &[
                            Block::Call(&format!("toolu_h{step}"), "hang", "{}", false),
                            Block::Call(&format!("toolu_e{step}"), "echo", "{}", false),
                        ],
                        Some("tool_use"),
                    ));
                    let turn = a.run_turn(user("hang"), d, &cancel);
                    let _ = tokio::time::timeout(Duration::from_millis(20), turn).await;
                    let history = a.transcript().to_vec();
                    a = agent(&ran, history);
                }
                7 => {
                    // Resumed from a record cut at any point (what a quit leaves).
                    let history = a.transcript().to_vec();
                    let keep = rng.usize(..=history.len());
                    a = agent(&ran, history[..keep].to_vec());
                }
                8 => {
                    // An older engine's damage: a message right after a call.
                    let mut history = a.transcript().to_vec();
                    if let Some(at) = history.iter().rposition(|m| m.role() == Role::Assistant) {
                        history.insert(at + 1, StoredMessage::user_text("a message between"));
                    }
                    a = agent(&ran, history);
                }
                9 => {
                    let keep = rng.usize(..=a.transcript().len());
                    drop(a.truncate(keep, None).await);
                }
                10 => drop(a.compact(None, d, &cancel).await),
                _ => drop(a.replace_context(user("the host's summary"), d).await),
            }
        }
        assert_eq!(model.refused(), Vec::<String>::new(), "seed {seed}");
        assert!(!model.requests.lock().expect("lock").is_empty());
    }
}
