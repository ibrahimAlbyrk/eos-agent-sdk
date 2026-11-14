//! One workflow run: its script realm on a thread of its own, its agents as
//! tasks on the host's runtime, its progress pump, and how it ends (record,
//! task events, the `<task-notification>`).
//!
//! The realm thread blocks on one channel of events — an agent or child
//! workflow settled, a timer fired, the run was stopped — and runs the
//! script's next stretch for each, so script code never runs on (or pins) a
//! runtime worker. Agents, timers and the pump are ordinary tasks that post
//! back to that channel.

mod agent;
mod call;
mod child;
mod finish;
mod hooks;
mod pump;
mod watch;

use finish::finish;
use pump::pump;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eos_agent_core::{BackgroundTask, Subagents, TaskEventKind, Workspace};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc as tmpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::WorkflowConfig;
use crate::engine::{Host, Realm, RealmSetup};
use crate::journal::{Entry, Journal, Lanes, Replay};
use crate::meta::Meta;
use crate::progress::{Progress, Row};
use crate::store::SessionDir;

/// Claude Code's `workflow-subagent` agent type.
pub const WORKFLOW_SUBAGENT: &str = "workflow-subagent";
/// What a workflow subagent never gets.
const DENIED: [&str; 3] = ["SendUserMessage", "Agent", "Workflow"];
const SUBAGENT: &str = include_str!("../../prompts/subagent.md");
const SUBAGENT_SCHEMA: &str = include_str!("../../prompts/subagent-schema.md");
const NOTE: &str = include_str!("../../prompts/subagent-note.md");
const NOTE_SCHEMA: &str = include_str!("../../prompts/subagent-note-schema.md");
/// Stalled agents are tried this many more times (Claude Code's `Zr`).
const STALL_RETRIES: u32 = 5;
/// Claude Code's `MAX_STRUCTURED_OUTPUT_RETRIES` default.
const STRUCTURED_RETRIES: u32 = 5;
/// Batches of progress rows go out this often, ticks alone at most every 10 s.
const BATCH: Duration = Duration::from_millis(16);
const TICKS_EVERY: Duration = Duration::from_secs(10);
/// Log lines kept for the run record.
const LOGS_KEPT: usize = 1000;
const REALM_STACK: usize = 8 << 20;
/// Claude Code's child-workflow group marker.
const CHILD_MARK: &str = "\u{25B8}";

/// Everything a run is started with.
#[derive(Debug, Clone)]
pub(crate) struct RunSpec {
    pub(crate) run_id: String,
    pub(crate) task_id: String,
    /// The agent that launched it (its session's files hold the run).
    pub(crate) session: String,
    pub(crate) tool_use_id: String,
    pub(crate) script: String,
    pub(crate) script_path: PathBuf,
    pub(crate) args: Option<Value>,
    pub(crate) meta: Meta,
    pub(crate) body: String,
    pub(crate) replay: Option<Replay>,
    pub(crate) dir: SessionDir,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) workspace: Option<Arc<Workspace>>,
    pub(crate) default_model: Option<String>,
}

/// How the realm thread ended.
#[derive(Debug)]
enum End {
    /// The script's result as JSON text (`None`: it returned `undefined`).
    Completed(Option<String>),
    Failed(String),
    Killed,
}

enum Event {
    Settle { realm: u32, call: u64, ok: bool, json: String },
    Timer { realm: u32, id: u64 },
    OpenChild { realm: u32, parent: u64, body: String, config: String, name: String },
    CloseChild { realm: u32 },
    Stop,
}

enum Pump {
    Row(Row),
    Finish(oneshot::Sender<(Progress, Vec<String>)>),
}

#[derive(Debug)]
struct Child {
    parent: u64,
    name: String,
    phase: String,
}

#[derive(Debug, Default)]
struct State {
    agents: u32,
    next_call: u64,
    next_timer: u64,
    next_realm: u32,
    phases: Vec<(String, u32, bool)>,
    current_phase: Option<String>,
    failures: Vec<String>,
    /// Replay is over: every later call runs live.
    live: bool,
    lanes: Lanes,
    timers: HashMap<(u32, u64), tokio::task::AbortHandle>,
    children: HashMap<u32, Child>,
    child_names: HashMap<String, u32>,
    outcome: Option<End>,
}

struct Shared {
    me: Weak<Shared>,
    spec: RunSpec,
    config: Arc<WorkflowConfig>,
    subagents: Arc<dyn Subagents>,
    journal: Journal,
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
    semaphore: Arc<Semaphore>,
    events: mpsc::Sender<Event>,
    rows: tmpsc::UnboundedSender<Pump>,
    budget_total: Option<u64>,
    state: Mutex<State>,
}

/// Starts a run in the background; `task` is how TaskStop reaches it.
pub(crate) fn start(
    spec: RunSpec,
    config: Arc<WorkflowConfig>,
    subagents: Arc<dyn Subagents>,
    task: Arc<BackgroundTask>,
) {
    let handle = tokio::runtime::Handle::current();
    let (events, inbox) = mpsc::channel();
    let (rows, pump_rx) = tmpsc::unbounded_channel();
    let budget_total = config.budgets.as_ref().and_then(|b| b.total(&spec.session));
    let journal = Journal::new(&spec.dir.transcripts(&spec.run_id));
    let started = Instant::now();
    let started_ms = epoch_ms();
    let shared = Arc::new_cyclic(|me| Shared {
        me: me.clone(),
        semaphore: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
        spec,
        config,
        subagents,
        journal,
        handle: handle.clone(),
        cancel: CancellationToken::new(),
        events,
        rows,
        budget_total,
        state: Mutex::new(State { next_realm: 1, ..State::default() }),
    });
    handle.spawn(pump(Arc::downgrade(&shared), pump_rx, started));

    // TaskStop.
    let stopper = shared.events.clone();
    let stop_task = Arc::clone(&task);
    let cancel = shared.cancel.clone();
    handle.spawn(async move {
        tokio::select! {
            () = stop_task.stop_requested().cancelled() => { let _ = stopper.send(Event::Stop); }
            () = cancel.cancelled() => {}
        }
    });

    let (done_tx, done_rx) = oneshot::channel();
    let runner = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name(format!("workflow-{}", runner.spec.run_id))
        .stack_size(REALM_STACK)
        .spawn(move || {
            let end = run_realms(&runner, inbox);
            let _ = done_tx.send(end);
        });
    if let Err(e) = thread {
        let _ = shared.events.send(Event::Stop);
        let message = format!("could not start the workflow thread: {e}");
        handle.spawn(finish(shared, End::Failed(message), task, started, started_ms));
        return;
    }
    handle.spawn(async move {
        shared
            .task_event(TaskEventKind::Started {
                task_type: "local_workflow".into(),
                description: shared.spec.meta.description.clone(),
                workflow_name: Some(shared.spec.meta.name.clone()),
                run_id: Some(shared.spec.run_id.clone()),
                transcript_dir: Some(shared.spec.dir.transcripts(&shared.spec.run_id).display().to_string()),
                command: None,
                output_file: None,
            })
            .await;
        let end = done_rx.await.unwrap_or_else(|_| End::Failed("the workflow thread ended unexpectedly".into()));
        finish(shared, end, task, started, started_ms).await;
    });
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- the realm thread ---------------------------------------------------

fn run_realms(shared: &Arc<Shared>, inbox: mpsc::Receiver<Event>) -> End {
    if shared.spec.replay.is_none() {
        let _ = shared.journal.append(&Entry::Launched);
    }
    {
        // Claude Code announces the declared phases up front.
        let mut state = lock(&shared.state);
        for phase in shared.spec.meta.phases.iter().flatten() {
            shared.announce(&mut state, &phase.title, None);
        }
    }
    let host: Arc<dyn Host> = Arc::clone(shared) as Arc<dyn Host>;
    let config = realm_config(shared.budget_total, shared.spec.args.as_ref(), None, 0);
    let setup = RealmSetup { id: 0, body: shared.spec.body.clone(), config, slice_limit: shared.config.slice_limit };
    let mut realms: HashMap<u32, Box<dyn Realm>> = HashMap::new();
    let mut main = match shared.config.engine.realm(setup, Arc::clone(&host)) {
        Ok(realm) => realm,
        Err(e) => return End::Failed(e.to_string()),
    };
    if let Err(e) = main.start() {
        return End::Failed(e.to_string());
    }
    realms.insert(0, main);
    loop {
        if let Some(end) = lock(&shared.state).outcome.take() {
            for realm in realms.values_mut() {
                realm.abort();
            }
            return end;
        }
        let Ok(event) = inbox.recv() else { return End::Failed("the workflow lost its event channel".into()) };
        let (realm_id, result) = match event {
            Event::Settle { realm, call, ok, json } => {
                (realm, realms.get_mut(&realm).map(|r| r.settle(call, ok, &json)))
            }
            Event::Timer { realm, id } => {
                lock(&shared.state).timers.remove(&(realm, id));
                (realm, realms.get_mut(&realm).map(|r| r.fire_timer(id)))
            }
            Event::OpenChild { realm, parent, body, config, name } => {
                let setup = RealmSetup { id: realm, body, config, slice_limit: shared.config.slice_limit };
                let opened =
                    shared.config.engine.realm(setup, Arc::clone(&host)).and_then(|mut r| r.start().map(|()| r));
                match opened {
                    Ok(r) => {
                        realms.insert(realm, r);
                        continue;
                    }
                    Err(e) => {
                        shared.child_failed(realm, parent, &name, "Error", &e.to_string());
                        continue;
                    }
                }
            }
            Event::CloseChild { realm } => {
                if let Some(mut r) = realms.remove(&realm) {
                    r.abort();
                }
                continue;
            }
            Event::Stop => {
                for realm in realms.values_mut() {
                    realm.abort();
                }
                return End::Killed;
            }
        };
        if let Some(Err(fatal)) = result {
            if realm_id == 0 {
                return End::Failed(fatal.to_string());
            }
            let child = lock(&shared.state).children.remove(&realm_id);
            if let Some(child) = child {
                realms.remove(&realm_id);
                shared.child_failed_known(&child, "Error", &fatal.to_string());
            }
        }
    }
}

/// What the prelude reads; `lane` is the lane the realm's script starts on
/// (a child workflow runs on the lane of its `workflow()` call).
fn realm_config(total: Option<u64>, args: Option<&Value>, child: Option<&str>, lane: usize) -> String {
    let mut config = serde_json::Map::new();
    config.insert("total".into(), total.map_or(Value::Null, Value::from));
    config.insert("lane".into(), lane.into());
    if let Some(args) = args {
        config.insert("args".into(), Value::String(args.to_string()));
    }
    if let Some(name) = child {
        config.insert("child".into(), serde_json::json!({ "name": name }));
    }
    Value::Object(config).to_string()
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("run_id", &self.spec.run_id).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::call::collapse;
    use super::finish::iso_time;
    use super::hooks::{format_error, thousands};
    use super::watch::tool_summary;

    #[test]
    fn times_and_numbers() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_762_600_000_123), "2025-11-08T11:06:40.123Z");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(collapse("  a \n b "), "a b");
        assert_eq!(
            format_error("Error", "boom", Some("    at f (workflow.js:1:2)\nnoise")),
            "Error: boom\n    at f (workflow.js:1:2)"
        );
        assert_eq!(tool_summary("Bash", r#"{"command":"ls  -la"}"#).as_deref(), Some("ls -la"));
    }
}
