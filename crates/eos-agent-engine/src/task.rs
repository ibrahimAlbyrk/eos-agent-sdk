//! One agent's task: it sleeps on its command channel between turns, and
//! after sitting idle long enough hands its history to the transcript store.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use eos_agent_core::{
    Agent, AgentEvent, BoxFuture, CancellationToken, Deps, EventSink, Reminders, StoredMessage, TranscriptStore,
    TurnError, Wake,
};
use eos_agent_types::ContentBlock;
use tokio::sync::mpsc;

use crate::Engine;
use crate::handle::{AgentUpdate, Command, CompactRequest, LocalRequest, Shared, TurnOverrides, TurnRequest};

/// Whether the agent's history is in memory, and whether the store is known
/// to hold exactly that history (checked once, at the first eviction).
#[derive(Default)]
struct Memory {
    evicted: bool,
    verified: bool,
    /// The store could not take the history; keep it in memory for good.
    pinned: bool,
}

pub(crate) async fn run(
    mut agent: Agent,
    engine: Engine,
    mut inbox: mpsc::Receiver<Command>,
    sink: ChannelSink,
    shared: Arc<Shared>,
    ready: Option<tokio::sync::oneshot::Receiver<()>>,
) {
    let mut memory = Memory::default();
    if let Some(ready) = ready {
        let _ = ready.await;
    }
    // A resumed conversation: tools rebuild what the agent had seen.
    agent.restore_tool_state().await;
    while let Some(command) = next(&mut inbox, &mut agent, &engine, &mut memory, &shared).await {
        let (input, overrides, reply) = match command {
            Command::Turn(turn) => {
                let TurnRequest { input, overrides, reply } = *turn;
                (input, overrides, Some(reply))
            }
            Command::Notify(text, false) => {
                agent.extensions().get_or_default::<Reminders>().push(text);
                shared.done();
                continue;
            }
            Command::Wake => {
                shared.wake_queued.store(false, Ordering::SeqCst);
                let Some(notes) = woken(&agent) else {
                    shared.done();
                    continue;
                };
                (notes, TurnOverrides::default(), None)
            }
            Command::Notify(text, true) => {
                (vec![ContentBlock::Text { text, citations: None }], TurnOverrides::default(), None)
            }
            Command::Compact(compact) => {
                let CompactRequest { focus, reply } = *compact;
                let result = match page_in(&mut agent, &engine, &mut memory, &shared).await {
                    Ok(()) => {
                        let token = begin(&shared);
                        let result = Box::pin(agent.compact(focus.as_deref(), deps(&engine, &sink), &token)).await;
                        end(&shared);
                        result
                    }
                    Err(e) => Err(TurnError::Store(e)),
                };
                shared.done();
                let _ = reply.send(result);
                continue;
            }
            Command::ReplaceContext(seed, reply) => {
                let result = match page_in(&mut agent, &engine, &mut memory, &shared).await {
                    Ok(()) => agent.replace_context(seed, deps(&engine, &sink)).await,
                    Err(e) => Err(TurnError::Store(e)),
                };
                shared.done();
                let _ = reply.send(result);
                continue;
            }
            Command::Reconfigure(update, reply) => {
                let AgentUpdate { config, system, toolbox } = *update;
                if let Some(config) = config {
                    *agent.config_mut() = config;
                }
                if let Some(system) = system {
                    agent.set_system(system);
                }
                if let Some(toolbox) = toolbox {
                    agent.set_toolbox(toolbox);
                }
                shared.done();
                let _ = reply.send(());
                continue;
            }
            Command::Rewind(keep, reply) => {
                let result = match page_in(&mut agent, &engine, &mut memory, &shared).await {
                    Ok(()) => agent.truncate(keep, engine.store.as_deref()).await,
                    Err(e) => Err(e.into()),
                };
                shared.done();
                let _ = reply.send(result);
                continue;
            }
            Command::Transcript(reply) => {
                let history = match (&engine.store, memory.evicted) {
                    (Some(store), true) => store.load(agent.id()).await,
                    _ => Ok(agent.transcript().to_vec()),
                };
                shared.done();
                let _ = reply.send(history);
                continue;
            }
            Command::Context(reply) => {
                let request = page_in(&mut agent, &engine, &mut memory, &shared).await.map(|()| agent.build_request());
                shared.done();
                let _ = reply.send(request);
                continue;
            }
            Command::Resume(reply) => {
                let result = match page_in(&mut agent, &engine, &mut memory, &shared).await {
                    Ok(()) => {
                        let token = begin(&shared);
                        let result = Box::pin(agent.resume_turn(deps(&engine, &sink), &token)).await;
                        end(&shared);
                        result
                    }
                    Err(e) => Err(TurnError::Store(e)),
                };
                shared.done();
                let _ = reply.send(result);
                continue;
            }
            Command::Local(local) => {
                let LocalRequest { work, reply } = *local;
                let result = match page_in(&mut agent, &engine, &mut memory, &shared).await {
                    Ok(()) => {
                        let token = begin(&shared);
                        let turn = work(token).await;
                        let result =
                            Box::pin(agent.record_turn(turn.input, turn.interrupted, deps(&engine, &sink))).await;
                        end(&shared);
                        result
                    }
                    Err(e) => Err(TurnError::Store(e)),
                };
                shared.done();
                let _ = reply.send(result);
                continue;
            }
        };
        if let Err(e) = page_in(&mut agent, &engine, &mut memory, &shared).await {
            shared.done();
            match reply {
                Some(reply) => drop(reply.send(Err(TurnError::Store(e)))),
                // A wake-up note still reaches the model with the next turn.
                None => agent.extensions().get_or_default::<Reminders>().push(text_of(input)),
            }
            continue;
        }
        let token = begin(&shared);
        let restore = overrides.apply(agent.config_mut());
        // Boxed, so the parked task is small between turns; the turn's own
        // state exists only while it runs.
        let result = Box::pin(agent.run_turn(input, deps(&engine, &sink), &token)).await;
        restore.apply(agent.config_mut());
        end(&shared);
        // Counted done before the reply, so a caller that sees the turn end also sees it idle.
        shared.done();
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }
}

/// The notes a wake asked for, as a turn's input; `None` when a request
/// already read them.
fn woken(agent: &Agent) -> Option<Vec<ContentBlock>> {
    if !agent.extensions().get::<Wake>().is_some_and(|w| w.take()) {
        return None;
    }
    let notes = agent.extensions().get_or_default::<Reminders>().take();
    (!notes.is_empty()).then(|| notes.into_iter().map(|text| ContentBlock::Text { text, citations: None }).collect())
}

/// The next command; while waiting, an agent idle past the engine's limit
/// releases its history.
async fn next(
    inbox: &mut mpsc::Receiver<Command>,
    agent: &mut Agent,
    engine: &Engine,
    memory: &mut Memory,
    shared: &Shared,
) -> Option<Command> {
    loop {
        let limit = match (&engine.store, engine.idle_eviction) {
            (Some(store), Some(limit)) if !memory.evicted && !memory.pinned && !agent.transcript().is_empty() => {
                Some((store, limit))
            }
            _ => None,
        };
        let Some((store, limit)) = limit else { return inbox.recv().await };
        match tokio::time::timeout(limit, inbox.recv()).await {
            Ok(command) => return command,
            Err(_) => evict(agent, &**store, memory, shared).await,
        }
    }
}

async fn evict(agent: &mut Agent, store: &dyn TranscriptStore, memory: &mut Memory, shared: &Shared) {
    if !memory.verified {
        // Histories handed in at spawn (a fork's, a host's) may not be on
        // disk yet; write them before trusting the store with them.
        let same = store.load(agent.id()).await.is_ok_and(|stored| same_bytes(&stored, agent.transcript()));
        if !same && store.replace(agent.id(), agent.transcript()).await.is_err() {
            memory.pinned = true;
            return;
        }
        memory.verified = true;
    }
    agent.release_history();
    memory.evicted = true;
    shared.evicted.store(true, Ordering::Relaxed);
}

async fn page_in(agent: &mut Agent, engine: &Engine, memory: &mut Memory, shared: &Shared) -> std::io::Result<()> {
    let (true, Some(store)) = (memory.evicted, &engine.store) else { return Ok(()) };
    let history = store.load(agent.id()).await?;
    agent.restore_history(history);
    memory.evicted = false;
    shared.evicted.store(false, Ordering::Relaxed);
    Ok(())
}

fn same_bytes(a: &[StoredMessage], b: &[StoredMessage]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.json() == y.json())
}

fn text_of(input: Vec<ContentBlock>) -> String {
    input
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn begin(shared: &Shared) -> CancellationToken {
    let token = CancellationToken::new();
    *shared.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(token.clone());
    token
}

fn end(shared: &Shared) {
    shared.live.set(None);
    *shared.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

fn deps<'a>(engine: &'a Engine, sink: &'a ChannelSink) -> Deps<'a> {
    Deps {
        model: &*engine.model,
        events: sink,
        permissions: &*engine.permissions,
        clock: &*engine.clock,
        store: engine.store.as_deref(),
    }
}

pub(crate) struct ChannelSink(pub(crate) mpsc::Sender<AgentEvent>);

impl EventSink for ChannelSink {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // A closed receiver means nobody listens; the agent carries on.
            let _ = self.0.send(event).await;
        })
    }
}
