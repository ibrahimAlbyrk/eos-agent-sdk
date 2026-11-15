//! Stopping a background subagent, by the model (TaskStop) and by the host
//! (`Sdk::stop_task`): the handles each takes (an agent id, which is also
//! the `<task-id>` of its notifications, and the name it was started with,
//! also written another way; a ListAgents `[ref]` is not a task handle and
//! is answered with the names that are), whose tasks a subagent may stop,
//! what stopping ends (its turn, its own commands, what a waiting one
//! started), what its parent and the host are told, what a later message
//! does, and the races with its wake and with a message.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Dirs, builder};
use eos_agent_mock_api::{MockApi, Reply, Step};
use eos_agent_sdk::{AgentOptions, Sdk, SdkAgent, StopTaskError, SubagentEvent, SubagentEventKind};
use eos_agent_types::{ContentBlock, Delta, MessageStart, Role, StopReason, StreamEvent, Usage};
use serde_json::{Value, json};
use tokio::sync::mpsc;

type Seen = Arc<Mutex<Vec<Value>>>;

fn texts(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|v| texts(v, out)),
        Value::Object(m) => m.values().for_each(|v| texts(v, out)),
        _ => {}
    }
}

fn text_of(body: &Value) -> String {
    let mut out = String::new();
    texts(&body["messages"], &mut out);
    out
}

/// The first message of the request's conversation: whose it is.
fn who(body: &Value) -> String {
    let mut out = String::new();
    texts(&body["messages"][0], &mut out);
    out
}

fn blocks(body: &Value) -> impl Iterator<Item = &Value> {
    body["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"].as_array()).flatten()
}

fn tool_results(body: &Value) -> usize {
    blocks(body).filter(|b| b["type"] == "tool_result").count()
}

/// The ids in the Agent results, in spawn order.
fn agent_ids(text: &str) -> Vec<String> {
    let ids = text.split("agentId: ").skip(1).map(|s| s.chars().take_while(char::is_ascii_alphanumeric).collect());
    let mut ids: Vec<String> = ids.collect();
    ids.dedup();
    ids
}

/// The id of the last background command a tool result announced.
fn shell_id(text: &str) -> Option<String> {
    let at = text.rfind("Command running in background with ID: ")?;
    Some(text[at + 39..].chars().take_while(char::is_ascii_alphanumeric).collect())
}

/// The subagent ids the top-level agent was given, in spawn order.
fn parent_ids(seen: &Seen) -> Vec<String> {
    let all = requests(seen);
    let last = all.iter().rev().find(|b| who(b).contains("PARENT-START")).expect("the parent asked");
    agent_ids(&text_of(last))
}

/// The `[ref]` ListAgents printed for `name`.
fn listed_ref(text: &str, name: &str) -> String {
    let at = text.rfind(&format!("{name} [")).expect("listed") + name.len() + 2;
    text[at..].chars().take_while(|c| *c != ']').collect()
}

fn call(id: &str, name: &str, input: &Value) -> Reply {
    Reply::tool_use(id, name, &input.to_string())
}

fn stop(n: usize, task_id: &str) -> Reply {
    call(&format!("toolu_stop{n}"), "TaskStop", &json!({ "task_id": task_id }))
}

fn load(tools: &str) -> Reply {
    call("toolu_search", "ToolSearch", &json!({ "query": format!("select:{tools}"), "max_results": 5 }))
}

fn spawn_agent(id: &str, name: &str, prompt: &str) -> Reply {
    let input = json!({
        "description": format!("Run {name}"),
        "prompt": prompt,
        "subagent_type": "general-purpose",
        "name": name,
        "run_in_background": true,
    });
    call(id, "Agent", &input)
}

fn bash_background(id: &str, command: &str) -> Reply {
    call(id, "Bash", &json!({ "command": command, "run_in_background": true, "description": "slow check" }))
}

/// A reply that writes `text`, then calls Bash in the foreground on
/// `command` (a turn stopped during the call ends with `text` as its last words).
fn text_then_bash(text: &str, command: &str) -> Reply {
    // Each call its own id, as the API gives them.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = format!("toolu_fg{}", CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let usage = Usage { input_tokens: Some(10), output_tokens: Some(1), ..Usage::default() };
    let start = MessageStart {
        id: "msg_mock".into(),
        model: "mock-model".into(),
        role: Role::Assistant,
        content: Vec::new(),
        stop_reason: None,
        usage,
    };
    let empty = serde_json::value::RawValue::from_string("{}".into()).expect("json");
    let input = json!({ "command": command }).to_string();
    let events = [
        StreamEvent::MessageStart(start),
        StreamEvent::ContentBlockStart { index: 0, block: ContentBlock::Text { text: String::new(), citations: None } },
        StreamEvent::ContentBlockDelta { index: 0, delta: Delta::Text(text.into()) },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::ContentBlockStart {
            index: 1,
            block: ContentBlock::ToolUse { id, name: "Bash".into(), input: empty },
        },
        StreamEvent::ContentBlockDelta { index: 1, delta: Delta::InputJson(input) },
        StreamEvent::ContentBlockStop { index: 1 },
        StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            stop_sequence: None,
            usage: Some(Usage { output_tokens: Some(5), ..Usage::default() }),
        },
        StreamEvent::MessageStop,
    ];
    Reply::Stream(events.into_iter().map(Step::Event).collect())
}

/// A mock answering with `respond(request text, request)`, every request kept.
async fn mock(seen: &Seen, respond: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static) -> MockApi {
    let log = Arc::clone(seen);
    MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| {
        let body: Value = serde_json::from_slice(b).unwrap_or_default();
        let reply = respond(&text_of(&body), &body);
        log.lock().expect("lock").push(body);
        reply
    })
    .await
    .expect("mock")
}

fn requests(seen: &Seen) -> Vec<Value> {
    seen.lock().expect("lock").clone()
}

fn count(seen: &Seen, want: impl Fn(&str) -> bool) -> usize {
    requests(seen).iter().filter(|b| want(&text_of(b))).count()
}

/// Waits (bounded) until some request's text satisfies `want`.
async fn wait_for(seen: &Seen, want: impl Fn(&str) -> bool) -> bool {
    for _ in 0..200 {
        if count(seen, &want) > 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// What the tool answered to the call `id`, in the last request holding it.
fn answer(seen: &Seen, id: &str) -> String {
    let all = requests(seen);
    let block = all
        .iter()
        .rev()
        .flat_map(|b| blocks(b).cloned().collect::<Vec<_>>())
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id);
    let mut out = String::new();
    texts(&block.unwrap_or_else(|| panic!("{id} answered"))["content"], &mut out);
    out.trim().to_owned()
}

async fn start(dirs: &Dirs, mock: &MockApi) -> (Sdk, SdkAgent, mpsc::Receiver<SubagentEvent>) {
    let sdk = builder(mock, dirs).build().await.expect("sdk");
    let (tx, rx) = mpsc::channel(1024);
    let options = AgentOptions::new(dirs.project()).mcp(false).subagent_events(tx);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    (sdk, agent, rx)
}

async fn turn(agent: &SdkAgent, text: &str) {
    tokio::time::timeout(Duration::from_secs(30), agent.send_text(text))
        .await
        .expect("the turn ends")
        .expect("alive")
        .expect("turn");
}

/// The status each `Ended` event of a subagent's turn carried, by agent id.
fn ended(rx: &mut mpsc::Receiver<SubagentEvent>) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let SubagentEventKind::Ended(end) = event.event {
            out.push((event.agent_id.to_string(), end.status));
        }
    }
    out
}

async fn settle(agent: &SdkAgent) {
    tokio::time::timeout(Duration::from_secs(20), agent.settle()).await.expect("settled in time");
}

const SCOUTS: [&str; 4] = ["scout-id", "scout-name", "Scout-Spelled", "scout-ref"];

#[tokio::test]
async fn task_stop_stops_a_background_subagent_by_its_id_and_its_name() {
    let dirs = Dirs::new("taskstop-handles");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            return Reply::text("scouting").slowed(Duration::from_secs(30));
        }
        let step = tool_results(body);
        if let Some(name) = SCOUTS.get(step) {
            return spawn_agent(&format!("toolu_agent{step}"), name, "CHILD-START look around");
        }
        let scout_ref = || listed_ref(text, "scout-ref");
        match step {
            4 => load("TaskStop"),
            5 => call("toolu_list", "ListAgents", &json!({})),
            6 => stop(6, &agent_ids(text)[0]),
            7 => stop(7, "scout-name"),
            8 => stop(8, "  scout-SPELLED "),
            9 => stop(9, &scout_ref()),
            10 => stop(10, &format!("scout-ref [{}]", scout_ref())),
            11 => stop(11, "scout-rfe"),
            12 => stop(12, "nobody"),
            13 => stop(13, "scout-ref"),
            14 => stop(14, &agent_ids(text)[0]),
            _ => Reply::text("stopped them all"),
        }
    })
    .await;
    let (_sdk, agent, mut events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;

    let ids = parent_ids(&seen);
    let result = |n: usize| answer(&seen, &format!("toolu_stop{n}"));
    for (n, (id, name)) in [(6, (&ids[0], SCOUTS[0])), (7, (&ids[1], SCOUTS[1])), (8, (&ids[2], SCOUTS[2]))] {
        let out: Value = serde_json::from_str(&result(n)).unwrap_or_else(|_| panic!("stopped: {}", result(n)));
        assert_eq!(
            out,
            json!({
                "message": format!("Successfully stopped task: {id} (Run {name})"),
                "task_id": id,
                "task_type": "local_agent",
                "command": format!("Run {name}"),
            }),
        );
    }
    // A `[ref]` addresses messages, not tasks: the answer lists the names.
    for n in [9, 10] {
        assert!(result(n).starts_with("<tool_use_error>No task found with ID: "), "{}", result(n));
        assert!(result(n).contains(". Running named agents: scout-ref</tool_use_error>"), "{}", result(n));
    }
    assert_eq!(
        result(11),
        "<tool_use_error>No task found with ID: scout-rfe. Did you mean: scout-ref?. Running named agents: \
         scout-ref</tool_use_error>"
    );
    assert_eq!(
        result(12),
        "<tool_use_error>No task found with ID: nobody. Running named agents: scout-ref</tool_use_error>"
    );
    assert!(result(13).contains(&format!("Successfully stopped task: {}", ids[3])), "{}", result(13));
    assert_eq!(result(14), format!("<tool_use_error>Task {} is not running (status: killed)</tool_use_error>", ids[0]));
    assert_eq!(agent.running_subagents(), 0, "every scout was stopped");

    // Each scout's parent is told it was stopped by the agent; the host sees each turn end as killed.
    settle(&agent).await;
    let told = requests(&seen).iter().map(text_of).collect::<String>();
    for id in &ids {
        let note = format!("<task-id>{id}</task-id>");
        let at = told.find(&note).unwrap_or_else(|| panic!("{id} reported"));
        let rest = &told[at..];
        assert!(rest.contains("<status>killed</status>") && rest.contains("was stopped by Claude"), "{rest}");
    }
    let ended = ended(&mut events);
    for id in &ids {
        assert!(ended.contains(&(id.clone(), "killed")), "{id}: {ended:?}");
    }
}

#[tokio::test]
async fn a_subagent_stops_only_its_own_tasks_and_the_session_stops_any() {
    let dirs = Dirs::new("taskstop-scope");
    let seen: Seen = Arc::default();
    // What every agent was shown, so one can name another's task.
    let all: Arc<Mutex<String>> = Arc::default();
    let shown = Arc::clone(&all);
    let mock = mock(&seen, move |text, body| {
        shown.lock().expect("lock").push_str(text);
        let step = tool_results(body);
        if who(body).contains("CHILD-B") {
            if let Some(id) = shell_id(text) {
                shown.lock().expect("lock").push_str(&format!("\nB-SHELL={id}\n"));
            }
            return match step {
                0 => bash_background("toolu_b_sh", "sleep 30"),
                _ => Reply::text("b works").slowed(Duration::from_secs(30)),
            };
        }
        if who(body).contains("CHILD-A") {
            let b = agent_ids(&shown.lock().expect("lock")).into_iter().nth(1).unwrap_or_default();
            return match step {
                0 => bash_background("toolu_a_sh", "sleep 30"),
                1 => load("TaskStop"),
                2 => stop(1, "worker-b"),
                3 => stop(2, &b),
                4 => stop(3, &shell_id(text).expect("its own command")),
                _ => Reply::text("a done"),
            };
        }
        match step {
            0 => spawn_agent("toolu_a", "worker-a", "CHILD-A"),
            1 => spawn_agent("toolu_b", "worker-b", "CHILD-B"),
            // Long enough for both to have started their commands.
            2 => call("toolu_wait", "Bash", &json!({ "command": "sleep 2" })),
            3 => load("TaskStop"),
            4 => {
                let all = shown.lock().expect("lock").clone();
                let b_shell = all.split("B-SHELL=").nth(1).and_then(|s| s.lines().next()).unwrap_or_default();
                stop(4, b_shell)
            }
            5 => stop(5, "worker-b"),
            _ => Reply::text("parent done"),
        }
    })
    .await;
    let (_sdk, agent, _events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    let ids = parent_ids(&seen);
    let (a, b) = (&ids[0], &ids[1]);
    assert!(wait_for(&seen, |t| t.contains("CHILD-A") && t.contains("toolu_stop3")).await);
    assert!(wait_for(&seen, |t| t.contains("CHILD-A") && t.contains("a done") || t.contains("toolu_stop3")).await);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let owned = format!("is owned by {b}; agent {a} cannot stop it.</tool_use_error>");
    assert_eq!(answer(&seen, "toolu_stop1"), format!("<tool_use_error>Task worker-b ({b}) {owned}"));
    assert_eq!(answer(&seen, "toolu_stop2"), format!("<tool_use_error>Task {b} {owned}"));
    let own: Value = serde_json::from_str(&answer(&seen, "toolu_stop3")).expect("its own command stopped");
    assert_eq!((own["task_type"].as_str(), own["command"].as_str()), (Some("local_bash"), Some("sleep 30")));
    let b_shell: Value = serde_json::from_str(&answer(&seen, "toolu_stop4")).expect("the session stops b's command");
    assert_eq!(b_shell["task_type"], "local_bash");
    let b_stopped: Value = serde_json::from_str(&answer(&seen, "toolu_stop5")).expect("the session stops b");
    assert_eq!(b_stopped["task_id"].as_str(), Some(b.as_str()));
}

#[tokio::test]
async fn stopping_a_subagent_that_waits_on_its_own_work_ends_that_work_and_what_it_started() {
    let dirs = Dirs::new("taskstop-waiting");
    let marker = dirs.root.join("marker");
    let command = format!("sleep 2; touch '{}'", marker.display());
    let seen: Seen = Arc::default();
    let mock = mock(&seen, move |text, body| {
        let step = tool_results(body);
        if who(body).contains("LEAF-START") {
            return Reply::text("leaf works").slowed(Duration::from_secs(30));
        }
        if who(body).contains("WAITER-START") {
            if text.contains("Background command \"slow check\"") {
                return Reply::text("WAITER-WOKEN");
            }
            return match step {
                0 => spawn_agent("toolu_leaf", "leaf", "LEAF-START"),
                1 => bash_background("toolu_sh", &command),
                _ => Reply::text("waiting on my check"),
            };
        }
        if text.contains("is waiting on its own background work") {
            return match step {
                1 => load("TaskStop"),
                2 => stop(1, "waiter"),
                _ => Reply::text("stopped the waiter"),
            };
        }
        match step {
            0 => spawn_agent("toolu_waiter", "waiter", "WAITER-START"),
            _ => Reply::text("parent waits"),
        }
    })
    .await;
    let (_sdk, agent, mut events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| t.contains("stopped the waiter") || t.contains("toolu_stop1")).await, "it stopped it");
    settle(&agent).await;
    let out: Value = serde_json::from_str(&answer(&seen, "toolu_stop1")).expect("stopped");
    assert_eq!(out["task_type"], "local_agent");
    assert_eq!(agent.running_subagents(), 0, "the subagent it started was stopped with it");

    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!marker.exists(), "its command was killed");
    assert_eq!(count(&seen, |t| t.contains("WAITER-WOKEN") || t.contains("completed (exit code")), 0, "no wake");
    // Its parent had been told it finished; nothing more, nor of what it started.
    assert_eq!(count(&seen, |t| t.contains("<status>killed</status>")), 0, "no notification of the stop");
    let ended = ended(&mut events);
    assert_eq!(ended.iter().filter(|(_, s)| *s == "killed").count(), 2, "both shown stopped: {ended:?}");
}

#[tokio::test]
async fn a_finished_subagent_is_not_running() {
    let dirs = Dirs::new("taskstop-finished");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            return Reply::text("found it");
        }
        match tool_results(body) {
            0 => spawn_agent("toolu_a", "quick", "CHILD-START"),
            _ if !text.contains("<status>completed</status>") => Reply::text("waiting"),
            1 => load("TaskStop"),
            2 => stop(1, "quick"),
            _ => Reply::text("done"),
        }
    })
    .await;
    let (_sdk, agent, _events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| t.contains("toolu_stop1")).await);
    settle(&agent).await;
    let id = &parent_ids(&seen)[0];
    assert_eq!(
        answer(&seen, "toolu_stop1"),
        format!("<tool_use_error>Task quick ({id}) is not running (status: completed)</tool_use_error>")
    );
}

#[tokio::test]
async fn after_task_stop_a_queued_message_is_dropped_and_a_new_one_resumes_it() {
    let dirs = Dirs::new("taskstop-message");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            if text.contains("RESUME-NOTE") {
                return Reply::text("RESUMED");
            }
            return Reply::text("helping").slowed(Duration::from_secs(30));
        }
        let to_helper =
            |id: &str, message: &str| call(id, "SendMessage", &json!({ "to": "helper", "message": message }));
        match tool_results(body) {
            0 => spawn_agent("toolu_a", "helper", "CHILD-START"),
            1 => load("TaskStop,SendMessage"),
            2 => to_helper("toolu_queued", "QUEUED-NOTE"),
            3 => stop(1, "helper"),
            4 => to_helper("toolu_resume", "RESUME-NOTE"),
            _ => Reply::text("parent done"),
        }
    })
    .await;
    let (_sdk, agent, _events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    settle(&agent).await;
    let resumed: Value = serde_json::from_str(&answer(&seen, "toolu_resume")).expect("answered");
    assert_eq!(resumed["success"], true, "{resumed}");
    assert!(resumed["message"].as_str().is_some_and(|m| m.starts_with("Resuming agent helper")), "{resumed}");
    assert!(wait_for(&seen, |t| t.contains("CHILD-START") && t.contains("RESUME-NOTE")).await, "resumed");
    let child = |t: &str| t.contains("CHILD-START") && !t.contains("PARENT-START");
    assert_eq!(count(&seen, |t| child(t) && t.contains("QUEUED-NOTE")), 0, "the queued message was dropped");
}

#[tokio::test]
async fn the_user_stops_a_subagent_and_its_idle_parent_is_woken_and_told() {
    let dirs = Dirs::new("taskstop-user");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            return text_then_bash("PARTIAL-NOTE so far", "sleep 30");
        }
        match tool_results(body) {
            0 => spawn_agent("toolu_a", "helper", "CHILD-START"),
            _ if !text.contains("was stopped by user") => Reply::text("parent waits"),
            1 => load("SendMessage"),
            2 => call("toolu_send", "SendMessage", &json!({ "to": "helper", "message": "go on" })),
            _ => Reply::text("parent done"),
        }
    })
    .await;
    let (sdk, agent, mut events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| t.contains("PARTIAL-NOTE") && t.contains("toolu_fg")).await, "the helper works");
    let id = parent_ids(&seen)[0].clone();

    // The host stops it by the name the model gave it.
    let stopped = sdk.stop_task(agent.id(), "helper").await.expect("stopped");
    assert_eq!((stopped.task_id.as_str(), stopped.agent_id.as_str()), (id.as_str(), id.as_str()));
    assert_eq!((stopped.task_type.as_str(), stopped.description.as_str()), ("local_agent", "Run helper"));
    assert!(wait_for(&seen, |t| t.contains("toolu_send")).await, "the idle parent was woken and answered");
    settle(&agent).await;
    let told = requests(&seen).iter().map(text_of).find(|t| t.contains("was stopped by user")).expect("told");
    assert!(told.contains("<status>killed</status>"), "{told}");
    assert!(told.contains("Agent \"Run helper\" was stopped by user"), "{told}");
    assert!(told.contains("<result>PARTIAL-NOTE so far</result>"), "its last words: {told}");
    let refused: Value = serde_json::from_str(&answer(&seen, "toolu_send")).expect("answered");
    assert_eq!(
        refused,
        json!({
            "success": false,
            "message": format!(
                "Agent {id} was stopped by the user and won't be resumed. Treat its work as cancelled; only launch \
                 a new agent if the user explicitly asks."
            ),
        })
    );
    assert!(ended(&mut events).contains(&(id.clone(), "killed")), "the host sees it stopped");
    assert_eq!(
        sdk.stop_task(agent.id(), &id).await,
        Err(StopTaskError::NotRunning { id: id.clone(), status: "killed" })
    );
    assert_eq!(sdk.stop_task(agent.id(), "nobody").await, Err(StopTaskError::NotFound("nobody".into())));
}

#[tokio::test]
async fn a_subagent_parent_is_woken_and_told_when_the_user_stops_its_subagent() {
    let dirs = Dirs::new("taskstop-nested");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if who(body).contains("LEAF-START") {
            return Reply::text("leaf works").slowed(Duration::from_secs(30));
        }
        if who(body).contains("MID-START") {
            if text.contains("was stopped by user") {
                return Reply::text("MID-HEARD");
            }
            return match tool_results(body) {
                0 => spawn_agent("toolu_leaf", "leaf", "LEAF-START"),
                _ => Reply::text("mid waits"),
            };
        }
        match tool_results(body) {
            0 => spawn_agent("toolu_mid", "mid", "MID-START"),
            _ => Reply::text("parent waits"),
        }
    })
    .await;
    let (sdk, agent, _events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| t.contains("LEAF-START")).await);
    assert!(wait_for(&seen, |t| t.contains("mid waits")).await, "mid went idle");
    sdk.stop_task(agent.id(), "leaf").await.expect("stopped");
    assert!(wait_for(&seen, |t| t.contains("MID-HEARD")).await, "mid was woken and told, then reported again");
    settle(&agent).await;
}

#[tokio::test]
async fn the_user_stops_a_subagent_in_its_woken_turn_and_no_wake_follows() {
    let dirs = Dirs::new("taskstop-woken");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            if text.contains("Background command \"slow check\"") {
                return Reply::text("WOKEN").slowed(Duration::from_secs(30));
            }
            return match tool_results(body) {
                0 => bash_background("toolu_sh", "sleep 0.3; echo done"),
                _ => Reply::text("waiting on my check"),
            };
        }
        match tool_results(body) {
            0 => spawn_agent("toolu_a", "checker", "CHILD-START"),
            _ => Reply::text("parent waits"),
        }
    })
    .await;
    let (sdk, agent, mut events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| !t.contains("PARENT-START") && t.contains("Background command \"slow check\"")).await);
    let begun = std::time::Instant::now();
    sdk.stop_task(agent.id(), "checker").await.expect("stopped in its woken turn");
    assert!(begun.elapsed() < Duration::from_secs(10), "its turn was interrupted");
    settle(&agent).await;
    let id = parent_ids(&seen)[0].clone();
    assert!(ended(&mut events).contains(&(id, "killed")));
    let told = requests(&seen).iter().map(text_of).filter(|t| t.contains("was stopped by user")).count();
    assert!(told > 0, "its parent was told");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let woken = count(&seen, |t| !t.contains("PARENT-START") && t.contains("Background command \"slow check\""));
    assert_eq!(woken, 1, "one woken turn, and none after the stop");
}

#[tokio::test]
async fn the_user_stops_a_foreground_subagent_and_its_parents_call_ends() {
    let dirs = Dirs::new("taskstop-foreground");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text, body| {
        if !text.contains("PARENT-START") {
            return Reply::text("slow").slowed(Duration::from_secs(30));
        }
        match tool_results(body) {
            0 => call(
                "toolu_fg_agent",
                "Agent",
                &json!({ "description": "Look", "prompt": "CHILD-START",
                    "subagent_type": "general-purpose", "run_in_background": false }),
            ),
            _ => Reply::text("parent done"),
        }
    })
    .await;
    let (sdk, agent, _events) = start(&dirs, &mock).await;
    let sending = agent.send_text("PARENT-START");
    let stopper = async {
        assert!(wait_for(&seen, |t| t.contains("CHILD-START") && !t.contains("PARENT-START")).await);
        let child = sdk.agents().into_iter().find(|a| a.parent_id.is_some()).expect("child").agent_id;
        sdk.stop_task(agent.id(), &child).await
    };
    let (done, stopped) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(sending, stopper) })
        .await
        .expect("the parent's call ended");
    done.expect("alive").expect("turn");
    let child = stopped.expect("stopped").task_id;
    assert_eq!(
        answer(&seen, "toolu_fg_agent"),
        format!("<tool_use_error>Agent {child} was stopped before it finished.</tool_use_error>")
    );
}

#[tokio::test]
async fn a_working_subagents_own_subagent_runs_on_and_reports_to_the_session() {
    let dirs = Dirs::new("taskstop-orphan");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |_text, body| {
        let first = who(body);
        if first.contains("LEAF-START") {
            return Reply::text("LEAF-DONE").slowed(Duration::from_millis(150));
        }
        if first.contains("MID-START") {
            return match tool_results(body) {
                0 => spawn_agent("toolu_leaf", "leaf", "LEAF-START"),
                _ => Reply::text("mid works").slowed(Duration::from_secs(30)),
            };
        }
        match tool_results(body) {
            0 => spawn_agent("toolu_mid", "mid", "MID-START"),
            _ => Reply::text("parent waits"),
        }
    })
    .await;
    let (sdk, agent, _events) = start(&dirs, &mock).await;
    turn(&agent, "PARENT-START").await;
    assert!(wait_for(&seen, |t| t.contains("LEAF-START") && t.contains("Async agent launched")).await);
    sdk.stop_task(agent.id(), "mid").await.expect("stopped while working");
    let to_main = |t: &str| t.starts_with("PARENT-START") && t.contains("<result>LEAF-DONE</result>");
    assert!(wait_for(&seen, to_main).await, "the leaf, not stopped with it, reported to the session");
    settle(&agent).await;
}
