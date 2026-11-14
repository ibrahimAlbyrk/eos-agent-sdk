//! An agent that ends its turn while background work of its own still runs
//! is woken by that work's `<task-notification>`, as Claude Code wakes it: a
//! top-level agent (a command, one moved to the background, a Monitor), a
//! background subagent (whose parent then hears from it a second time), and
//! a subagent whose own subagent reports, even after it was let go. A
//! stopped subagent and an ended session are not woken.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Dirs, builder};
use eos_agent_engine::JsonlStore;
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, SdkBuilder};
use serde_json::Value;

type Seen = Arc<Mutex<Vec<Value>>>;

const BASH: &str = r#"{"command":"sleep 1; echo done","run_in_background":true,"description":"slow check"}"#;
const AGENT: &str = r#"{"description":"Checker","prompt":"CHILD-START run the check","subagent_type":"general-purpose","run_in_background":true}"#;

fn text_of(body: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Value::Object(m) => m.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(&body["messages"], &mut out);
    out
}

/// The shell's completion notice (not a subagent's).
fn shell_done(text: &str) -> bool {
    text.contains("Background command \"slow check\" completed")
}

async fn mock(seen: &Seen, respond: fn(&str) -> Reply) -> MockApi {
    let log = Arc::clone(seen);
    MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| {
        let body: Value = serde_json::from_slice(b).unwrap_or_default();
        let text = text_of(&body);
        log.lock().expect("lock").push(body);
        respond(&text)
    })
    .await
    .expect("mock")
}

/// Waits (bounded) until some request's text satisfies `want`.
async fn wait_for(seen: &Seen, want: impl Fn(&str) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if seen.lock().expect("lock").iter().any(|b| want(&text_of(b))) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn a_top_level_agent_wakes_when_its_background_command_ends() {
    let dirs = Dirs::new("wake-main");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if shell_done(text) {
            Reply::text("MAIN-WOKE")
        } else if text.contains("Command running in background") {
            Reply::text("Waiting for the check notification.")
        } else {
            Reply::tool_use("toolu_sh", "Bash", BASH)
        }
    })
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("MAIN-START").await.expect("alive").expect("turn");

    assert!(
        wait_for(&seen, shell_done).await,
        "nothing woke the agent: the command's notice sits in its reminders until someone sends a message"
    );
}

#[tokio::test]
async fn a_background_subagent_wakes_when_its_background_command_ends() {
    let dirs = Dirs::new("wake-sub");
    subagent_wakes(&dirs, builder_for).await;
}

/// With a short release time, a subagent whose command still runs is kept
/// (letting it go would kill the command) and woken as before.
#[tokio::test]
async fn a_subagent_with_a_running_command_is_not_let_go() {
    let dirs = Dirs::new("wake-keep");
    subagent_wakes(&dirs, releasing).await;
}

fn builder_for(mock: &MockApi, dirs: &Dirs) -> SdkBuilder {
    builder(mock, dirs)
}

/// Lets finished subagents go after 50 ms idle (with a transcript store).
fn releasing(mock: &MockApi, dirs: &Dirs) -> SdkBuilder {
    builder(mock, dirs)
        .store(Arc::new(JsonlStore::new(dirs.root.join("transcripts"))))
        .idle_eviction(Some(Duration::from_millis(50)))
}

async fn subagent_wakes(dirs: &Dirs, make: fn(&MockApi, &Dirs) -> SdkBuilder) {
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if !text.contains("PARENT-START") {
            // The subagent.
            return if shell_done(text) {
                Reply::text("CHILD-WOKE check passed")
            } else if text.contains("Command running in background") {
                Reply::text("Waiting for the check notification.")
            } else {
                Reply::tool_use("toolu_sh", "Bash", BASH)
            };
        }
        if text.contains("CHILD-WOKE") {
            Reply::text("parent read the final result")
        } else if text.contains("<task-notification>") {
            Reply::text("parent noted the interim result")
        } else if text.contains("Async agent launched") {
            Reply::text("parent waits")
        } else {
            Reply::tool_use("toolu_ag", "Agent", AGENT)
        }
    })
    .await;
    let sdk = make(&mock, dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("PARENT-START").await.expect("alive").expect("turn");

    assert!(
        wait_for(&seen, |t| t.contains("PARENT-START")
            && t.contains("<result>Waiting for the check notification.</result>"))
        .await,
        "the subagent ran its command, ended its turn, and the parent got its interim result"
    );
    assert!(
        wait_for(&seen, |t| !t.contains("PARENT-START") && shell_done(t)).await,
        "nothing woke the subagent: its command's notice sits in its reminders until its parent messages it"
    );
    assert!(
        wait_for(&seen, |t| t.contains("PARENT-START") && t.contains("CHILD-WOKE")).await,
        "the parent hears from the subagent a second time"
    );
    let notes = seen.lock().expect("lock").iter().map(text_of).find(|t| t.contains("<result>Waiting for"));
    assert!(
        notes.is_some_and(|t| t.contains("may resume on its own")),
        "the parent is told the subagent still has background work of its own (Claude Code's note)"
    );
}

/// Counts the requests whose text satisfies `want`.
fn count(seen: &Seen, want: impl Fn(&str) -> bool) -> usize {
    seen.lock().expect("lock").iter().filter(|b| want(&text_of(b))).count()
}

#[tokio::test]
async fn a_top_level_agent_wakes_for_its_monitors_events() {
    const MONITOR: &str = r#"{"command":"sleep 0.3; echo READY-$((40+2))","description":"watch","timeout_ms":5000}"#;
    let dirs = Dirs::new("wake-monitor");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if text.contains("READY-42") {
            Reply::text("MON-WOKE")
        } else if text.contains("Monitor started") {
            Reply::text("Watching.")
        } else {
            Reply::tool_use("toolu_mon", "Monitor", MONITOR)
        }
    })
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("MAIN-START").await.expect("alive").expect("turn");
    assert!(
        wait_for(&seen, |t| t.contains("<task-notification>") && t.contains("READY-42")).await,
        "the monitor's event woke the agent"
    );
}

#[tokio::test]
async fn a_command_moved_to_the_background_wakes_the_agent_when_it_ends() {
    const SLOW: &str = r#"{"command":"sleep 1; echo done","timeout":200,"description":"slow check"}"#;
    let dirs = Dirs::new("wake-moved");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if shell_done(text) {
            Reply::text("MAIN-WOKE")
        } else if text.contains("moved to the background") {
            Reply::text("It is still running.")
        } else {
            Reply::tool_use("toolu_sh", "Bash", SLOW)
        }
    })
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("MAIN-START").await.expect("alive").expect("turn");
    assert!(wait_for(&seen, shell_done).await, "the moved command's notice woke the agent");
}

const CHILD: &str = r#"{"description":"Child","prompt":"CHILD-START start a helper","subagent_type":"general-purpose","run_in_background":true}"#;
const GRANDCHILD: &str = r#"{"description":"Grandchild","prompt":"GRAND-START look it up","subagent_type":"general-purpose","run_in_background":true}"#;

/// main → child (background) → grandchild (background, slow). The child
/// ends its turn first; the grandchild's report wakes it, and main hears
/// from the child a second time.
async fn nested(dirs: &Dirs, make: fn(&MockApi, &Dirs) -> SdkBuilder) -> (Seen, Vec<eos_agent_sdk::AgentInfo>) {
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if text.contains("MAIN-START") {
            return if text.contains("CHILD-AGAIN") {
                Reply::text("main read the final result")
            } else if text.contains("<task-notification>") {
                Reply::text("main noted the interim result")
            } else if text.contains("Async agent launched") {
                Reply::text("main waits")
            } else {
                Reply::tool_use("toolu_child", "Agent", CHILD)
            };
        }
        if text.contains("CHILD-START") {
            return if text.contains("GRAND-DONE") {
                Reply::text("CHILD-AGAIN with the helper's answer")
            } else if text.contains("Async agent launched") {
                Reply::text("child waits for its helper")
            } else {
                Reply::tool_use("toolu_grand", "Agent", GRANDCHILD)
            };
        }
        Reply::text("GRAND-DONE").slowed(Duration::from_millis(150))
    })
    .await;
    let sdk = make(&mock, dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("MAIN-START").await.expect("alive").expect("turn");
    assert!(
        wait_for(&seen, |t| t.contains("MAIN-START") && t.contains("<result>child waits for its helper</result>"))
            .await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let during = sdk.agents();
    assert!(
        wait_for(&seen, |t| t.contains("MAIN-START") && t.contains("CHILD-AGAIN")).await,
        "the grandchild's report woke the child, which reported to main again"
    );
    assert_eq!(count(&seen, |t| !t.contains("CHILD-START") && !t.contains("MAIN-START")), 1, "one grandchild turn");
    (seen, during)
}

#[tokio::test]
async fn a_subagents_own_subagent_wakes_it() {
    let dirs = Dirs::new("wake-nested");
    nested(&dirs, builder_for).await;
}

#[tokio::test]
async fn a_report_to_a_parent_that_was_let_go_restarts_it() {
    let dirs = Dirs::new("wake-released");
    let (_seen, during) = nested(&dirs, releasing).await;
    let child = during.iter().find(|a| a.description.as_deref() == Some("Child")).expect("child listed");
    assert!(child.evicted && !child.busy, "the child was let go while its helper worked");
}

#[tokio::test]
async fn a_subagent_the_user_stopped_is_not_woken() {
    let dirs = Dirs::new("wake-stopped");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if !text.contains("PARENT-START") {
            return if shell_done(text) {
                Reply::text("CHILD-WOKE")
            } else if text.contains("Command running in background") {
                Reply::text("still thinking").slowed(Duration::from_secs(5))
            } else {
                Reply::tool_use("toolu_sh", "Bash", BASH)
            };
        }
        if text.contains("Async agent launched") || text.contains("<task-notification>") {
            Reply::text("parent waits")
        } else {
            Reply::tool_use("toolu_ag", "Agent", AGENT)
        }
    })
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("PARENT-START").await.expect("alive").expect("turn");
    assert!(wait_for(&seen, |t| !t.contains("PARENT-START") && t.contains("Command running in background")).await);
    let child = sdk.agents().into_iter().find(|a| a.parent_id.is_some()).expect("child").agent_id;
    sdk.stop_task(agent.id(), &child).await.expect("stopped");
    // The command ends about a second after it started.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(count(&seen, |t| !t.contains("PARENT-START") && shell_done(t)), 0, "the stopped child stayed stopped");
}

#[tokio::test]
async fn ending_the_session_ends_its_commands_and_wakes_nothing() {
    let dirs = Dirs::new("wake-end");
    let marker = dirs.root.join("marker");
    let command = serde_json::json!({
        "command": format!("sleep 1; touch '{}'", marker.display()),
        "run_in_background": true,
        "description": "slow check",
    })
    .to_string();
    let seen: Seen = Arc::default();
    let reply = Arc::new(command);
    let log = Arc::clone(&seen);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| {
        let body: Value = serde_json::from_slice(b).unwrap_or_default();
        let text = text_of(&body);
        log.lock().expect("lock").push(body);
        if text.contains("Command running in background") {
            Reply::text("Waiting.")
        } else {
            Reply::tool_use("toolu_sh", "Bash", &reply)
        }
    })
    .await
    .expect("mock");
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("MAIN-START").await.expect("alive").expect("turn");
    let before = count(&seen, |_| true);
    drop(agent);
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(count(&seen, |_| true), before, "nothing ran after the session ended");
    assert!(!marker.exists(), "the command died with its session");
}

/// A message from the parent and the child's own command notice both come
/// while the child works: neither starts a run of its own beside the
/// working one, and the one wake after it reads both.
#[tokio::test]
async fn a_message_and_a_notice_for_a_working_subagent_make_one_more_turn() {
    const NAMED: &str = r#"{"description":"Checker","name":"checker","prompt":"CHILD-START run the check","subagent_type":"general-purpose","run_in_background":true}"#;
    const QUICK: &str = r#"{"command":"sleep 0.4; echo done","run_in_background":true,"description":"slow check"}"#;
    const PING: &str = r#"{"to":"checker","message":"PING-MSG also check the docs"}"#;
    let dirs = Dirs::new("wake-message");
    let seen: Seen = Arc::default();
    let mock = mock(&seen, |text| {
        if !text.contains("PARENT-START") {
            return match (text.contains("PING-MSG"), shell_done(text)) {
                (true, true) => Reply::text("CHILD-SECOND"),
                (true, false) | (false, true) => Reply::text("CHILD-PARTIAL"),
                _ if text.contains("Command running in background") => {
                    Reply::text("CHILD-FIRST").slowed(Duration::from_millis(220))
                }
                _ => Reply::tool_use("toolu_sh", "Bash", QUICK),
            };
        }
        if text.contains("CHILD-SECOND") {
            Reply::text("parent read the final result")
        } else if text.contains("<task-notification>") || text.contains("toolu_msg") {
            Reply::text("parent waits")
        } else if text.contains("Async agent launched") {
            // Sent while the child's last request streams (the mock waits before each event).
            Reply::tool_use("toolu_msg", "SendMessage", PING).slowed(Duration::from_millis(90))
        } else {
            Reply::tool_use("toolu_ag", "Agent", NAMED)
        }
    })
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("PARENT-START").await.expect("alive").expect("turn");
    assert!(wait_for(&seen, |t| t.contains("PARENT-START") && t.contains("CHILD-SECOND")).await);
    tokio::time::timeout(Duration::from_secs(10), agent.settle()).await.expect("settled");
    let child = |t: &str| !t.contains("PARENT-START");
    assert_eq!(count(&seen, child), 3, "start, the command's result, then one turn for the message and the notice");
    assert_eq!(count(&seen, |t| child(t) && t.contains("CHILD-PARTIAL")), 0);
}
