//! One client connection: its SDK (built by `initialize`), its agents, and
//! the methods it can call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eos_agent_anthropic::auth::{self, ClaudeLogin, Credential, StaticCredential, TokenSource};
use eos_agent_core::{TurnError, TurnOutcome};
use eos_agent_engine::{Gone, JsonlStore, TurnResult};
use eos_agent_sdk::ChatGptLogin;
use eos_agent_sdk::{
    AgentInfo, CommandInfo, CompleteError, CompleteRequest, ControlError, InputError, ReadOnly, Sdk, SdkAgent,
    SkillOrigin, SubagentEvent, TaskEvent,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{OnceCell, mpsc, oneshot};

use crate::events::{Flush, pump_agent, pump_subagents, pump_tasks};
use crate::host::{ClientAuthPrompter, ClientGate, ClientPrompter};
use crate::params::{self, Auth, Permissions};
use crate::rpc::{self, Incoming, Peer, RpcError};

const OUTGOING_LINES: usize = 1024;
const SUBAGENT_EVENTS: usize = 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// What `initialize` sets up.
struct Ready {
    sdk: Sdk,
    store: Option<JsonlStore>,
    subagent_events: mpsc::Sender<SubagentEvent>,
    subagent_flush: mpsc::Sender<Flush>,
    task_events: mpsc::Sender<TaskEvent>,
}

/// A top-level agent and the way to flush its event pump.
struct Hosted {
    agent: SdkAgent,
    flush: mpsc::Sender<Flush>,
}

struct Session {
    peer: Arc<Peer>,
    ready: OnceCell<Ready>,
    agents: Mutex<HashMap<String, Arc<Hosted>>>,
}

/// Serves one client until it closes its side; its agents stop with it.
pub(crate) async fn serve(input: impl AsyncRead + Unpin, output: impl AsyncWrite + Unpin + Send + 'static) {
    let (lines_tx, lines_rx) = mpsc::channel(OUTGOING_LINES);
    let writer = tokio::spawn(rpc::write_lines(lines_rx, output));
    let session =
        Arc::new(Session { peer: Arc::new(Peer::new(lines_tx)), ready: OnceCell::new(), agents: Mutex::default() });
    let mut lines = BufReader::new(input).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            Arc::clone(&session).incoming(&line).await;
        }
    }
    session.peer.close();
    let agents: Vec<Arc<Hosted>> = session.agents().drain().map(|(_, a)| a).collect();
    for hosted in &agents {
        hosted.agent.interrupt();
    }
    drop(agents);
    drop(session);
    // Interrupted turns end quickly; output still queued gets this long.
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, writer).await;
}

impl Session {
    fn agents(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Hosted>>> {
        self.agents.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn incoming(self: Arc<Self>, line: &str) {
        let message: Incoming = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                let id = serde_json::value::RawValue::NULL.to_owned();
                self.peer.respond(&id, Err(RpcError::new(rpc::PARSE_ERROR, e.to_string()))).await;
                return;
            }
        };
        match (message.id, message.method) {
            (Some(id), Some(method)) => {
                tokio::spawn(async move {
                    let result = self.call(&method, message.params).await;
                    self.peer.respond(&id, result).await;
                });
            }
            // Notifications from the client carry nothing the server acts on.
            (None, Some(_)) => {}
            (Some(id), None) => {
                let outcome = match message.error {
                    Some(error) => Err(error),
                    None => Ok(message.result.unwrap_or(Value::Null)),
                };
                self.peer.resolve(&id, outcome);
            }
            (None, None) => {
                let id = serde_json::value::RawValue::NULL.to_owned();
                self.peer
                    .respond(&id, Err(RpcError::new(rpc::INVALID_REQUEST, "neither a request nor a response")))
                    .await;
            }
        }
    }

    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        if method == "initialize" {
            return self.initialize(params::parse(params)?).await;
        }
        let ready = self.ready.get().ok_or_else(|| RpcError::new(rpc::NOT_INITIALIZED, "call initialize first"))?;
        match method {
            "agent.spawn" => self.spawn(ready, params::parse(params)?).await,
            "agent.send" => self.send(ready, params::parse(params)?).await,
            "agent.interrupt" => {
                let p: params::AgentId = params::parse(params)?;
                if !ready.sdk.interrupt(&p.agent_id) {
                    return Err(unknown(&p.agent_id));
                }
                Ok(json!({}))
            }
            "agent.notify" => {
                let p: params::Notify = params::parse(params)?;
                self.hosted(&p.agent_id)?.agent.notify(p.text, p.wake).await.map_err(|g| gone(&g))?;
                Ok(json!({}))
            }
            "agent.compact" => {
                let p: params::Compact = params::parse(params)?;
                let hosted = self.hosted(&p.agent_id)?;
                let result = hosted.agent.compact(p.focus).await.map_err(|g| gone(&g))?;
                flush(ready, &hosted).await;
                let compacted = result.map_err(|e| failed(&e))?;
                Ok(json!({ "compacted": compacted }))
            }
            "agent.setModel" => {
                let p: params::SetModel = params::parse(params)?;
                let hosted = self.hosted(&p.agent_id)?;
                if let Some(model) = &p.model {
                    hosted.agent.set_model(model).await.map_err(|e| control_failed(&e))?;
                }
                if let Some(effort) = &p.effort {
                    hosted.agent.set_effort(effort.as_deref()).await.map_err(|g| gone(&g))?;
                }
                Ok(json!({ "model": hosted.agent.model(), "effort": hosted.agent.effort() }))
            }
            "agent.rewind" => {
                let p: params::Rewind = params::parse(params)?;
                self.hosted(&p.agent_id)?.agent.rewind(p.keep).await.map_err(|e| control_failed(&e))?;
                Ok(json!({}))
            }
            "agent.commands" => {
                let p: params::AgentId = params::parse(params)?;
                let commands = self.hosted(&p.agent_id)?.agent.commands();
                Ok(Value::Array(commands.iter().map(command_json).collect()))
            }
            "task.stop" => {
                let p: params::StopTask = params::parse(params)?;
                let stopped = self.hosted(&p.agent_id)?.agent.stop_task(&p.task_id).await;
                let stopped = stopped.map_err(|e| RpcError::new(rpc::INVALID_REQUEST, e.to_string()))?;
                Ok(json!({
                    "agentId": stopped.agent_id, "taskId": stopped.task_id, "taskType": stopped.task_type,
                    "description": stopped.description,
                }))
            }
            "complete" => complete(ready, params::parse(params)?).await,
            "agent.list" => Ok(Value::Array(ready.sdk.agents().iter().map(info_json).collect())),
            "mcp.status" => {
                let p: params::AgentId = params::parse(params)?;
                let servers = self.hosted(&p.agent_id)?.agent.mcp_status();
                Ok(Value::Array(servers.iter().map(mcp_status_json).collect()))
            }
            "mcp.reconnect" => {
                let p: params::McpServer = params::parse(params)?;
                self.hosted(&p.agent_id)?
                    .agent
                    .mcp_reconnect(&p.server)
                    .await
                    .map_err(|e| RpcError::new(rpc::INVALID_REQUEST, e))?;
                Ok(json!({}))
            }
            "mcp.signOut" => {
                let p: params::McpServer = params::parse(params)?;
                let signed_out = self
                    .hosted(&p.agent_id)?
                    .agent
                    .mcp_sign_out(&p.server)
                    .await
                    .map_err(|e| RpcError::new(rpc::INVALID_REQUEST, e))?;
                Ok(json!({ "signedOut": signed_out }))
            }
            "agent.close" => {
                let p: params::AgentId = params::parse(params)?;
                let hosted = self.agents().remove(&p.agent_id).ok_or_else(|| unknown(&p.agent_id))?;
                hosted.agent.interrupt();
                Ok(json!({}))
            }
            other => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("no method {other}"))),
        }
    }

    async fn initialize(&self, p: params::Initialize) -> Result<Value, RpcError> {
        let mut first = false;
        self.ready
            .get_or_try_init(|| {
                first = true;
                self.build(p)
            })
            .await?;
        if !first {
            return Err(RpcError::new(rpc::INVALID_REQUEST, "already initialized"));
        }
        Ok(json!({
            "protocolVersion": 1,
            "server": { "name": "eos-agent-server", "version": env!("CARGO_PKG_VERSION") },
        }))
    }

    async fn build(&self, p: params::Initialize) -> Result<Ready, RpcError> {
        let auths = p.auth.as_ref().map(params::Auths::all).unwrap_or_default();
        let openai_store = auths.iter().find_map(|a| match a {
            Auth::OpenAiStore(dir) => Some(dir),
            _ => None,
        });
        let claude: Vec<&Auth> = auths.iter().filter(|a| !matches!(a, Auth::OpenAiStore(_))).collect();
        if claude.len() > 1 {
            return Err(RpcError::params("auth: give one Claude credential"));
        }
        let claude_auth = match claude.first() {
            Some(auth) => Some(token_source(auth)?),
            // Without a ChatGPT store, Claude as before; with one, only when asked for.
            None if openai_store.is_none() => {
                Some(auth::from_env().unwrap_or_else(|| Arc::new(ClaudeLogin::keychain(None))))
            }
            None => None,
        };
        let secs = |ms: Option<u64>, default: u64| Duration::from_millis(ms.unwrap_or(default));
        let mut builder = Sdk::builder().prompter(Arc::new(ClientPrompter {
            peer: Arc::clone(&self.peer),
            timeout: secs(p.ask_timeout_ms, 10 * 60 * 1000),
        }));
        if let Some(auth) = claude_auth {
            builder = builder.auth(auth);
        }
        if let Some(dir) = openai_store {
            builder = builder.chatgpt_store(dir);
            if let Some(url) = &p.openai_base_url {
                builder = builder.openai_base_url(url);
            }
        }
        let step_up = matches!(p.permissions, Permissions::Ask).then(|| secs(p.ask_timeout_ms, 10 * 60 * 1000));
        builder = builder.mcp_prompter(Arc::new(ClientAuthPrompter { peer: Arc::clone(&self.peer), step_up }));
        if let Some(Auth::ClaudeStore(dir)) = claude.first() {
            builder = builder.mcp_credentials(eos_agent_sdk::claude_mcp_credentials(Some(dir)));
        }
        builder = match p.permissions {
            Permissions::Ask => builder.permissions(Arc::new(ClientGate {
                peer: Arc::clone(&self.peer),
                timeout: secs(p.permission_timeout_ms, 60 * 1000),
            })),
            Permissions::ReadOnly => builder.permissions(Arc::new(ReadOnly)),
            Permissions::Allow => builder,
        };
        if let Some(url) = &p.base_url {
            builder = builder.base_url(url);
        }
        if let Some(model) = &p.model {
            builder = builder.model(model);
        }
        if let Some(dir) = &p.data_dir {
            builder = builder.data_dir(dir);
        }
        if let Some(auth) = &p.artifacts_auth {
            if matches!(auth, Auth::OpenAiStore(_)) {
                return Err(RpcError::params("artifactsAuth: the Artifact tools take only a Claude credential"));
            }
            builder = builder.artifact_auth(token_source(auth)?);
        }
        if let Some(on) = p.artifacts {
            builder = builder.artifacts(on);
        }
        if let Some(on) = p.workflows {
            builder = builder.workflows(on);
        }
        if let Some(dir) = p.workflow_dir {
            builder = builder.workflow_dir(dir);
        }
        if let Some(sandbox) = p.sandbox {
            builder = builder.sandbox(sandbox);
        }
        if let Some(secs) = p.idle_eviction_secs {
            builder = builder.idle_eviction((secs > 0).then(|| Duration::from_secs(secs)));
        }
        let store = p.transcripts.as_ref().map(JsonlStore::new);
        if let Some(store) = &store {
            builder = builder.store(Arc::new(store.clone()));
        }
        let sdk = builder.build().await.map_err(|e| RpcError::new(rpc::INVALID_REQUEST, e.to_string()))?;
        let (subagent_events, events_rx) = mpsc::channel(SUBAGENT_EVENTS);
        let (subagent_flush, flush_rx) = mpsc::channel(16);
        tokio::spawn(pump_subagents(Arc::clone(&self.peer), events_rx, flush_rx));
        let (task_events, tasks_rx) = mpsc::channel(SUBAGENT_EVENTS);
        tokio::spawn(pump_tasks(Arc::clone(&self.peer), tasks_rx));
        Ok(Ready { sdk, store, subagent_events, subagent_flush, task_events })
    }

    async fn spawn(&self, ready: &Ready, p: params::Spawn) -> Result<Value, RpcError> {
        let history = match (p.resume, &p.id, &ready.store) {
            (false, ..) => Vec::new(),
            (true, Some(id), Some(store)) => {
                store.load(id).await.map_err(|e| RpcError::new(rpc::INVALID_REQUEST, format!("reading {id}: {e}")))?
            }
            (true, None, _) => return Err(RpcError::params("resume needs the id of the agent to continue")),
            (true, _, None) => return Err(RpcError::params("resume needs `transcripts` in initialize")),
        };
        if let Some(id) = &p.id
            && self.agents().contains_key(id)
        {
            return Err(RpcError::params(format!("agent {id} is already running")));
        }
        let mut options = p.options()?.history(history);
        options.subagent_events = Some(ready.subagent_events.clone());
        options.task_events = Some(ready.task_events.clone());
        let (agent, events) =
            ready.sdk.spawn(options).await.map_err(|e| RpcError::new(rpc::INVALID_REQUEST, e.to_string()))?;
        let id = agent.id().to_owned();
        let (flush, flushes) = mpsc::channel(4);
        tokio::spawn(pump_agent(Arc::clone(&self.peer), id.clone(), events, flushes));
        self.agents().insert(id.clone(), Arc::new(Hosted { agent, flush }));
        Ok(json!({ "agentId": id }))
    }

    async fn send(&self, ready: &Ready, p: params::Send) -> Result<Value, RpcError> {
        let hosted = self.hosted(&p.agent_id)?;
        let result = match (p.text, p.content) {
            (Some(text), None) => match hosted.agent.send_input(&text).await {
                Ok(result) => result,
                Err(InputError::Gone(g)) => return Err(gone(&g)),
                Err(InputError::Command(e)) => return Err(RpcError::params(e)),
            },
            (None, Some(blocks)) if !blocks.is_empty() => hosted.agent.send(blocks).await.map_err(|g| gone(&g))?,
            _ => return Err(RpcError::params("give exactly one of text or content (non-empty)")),
        };
        flush(ready, &hosted).await;
        outcome(result)
    }

    fn hosted(&self, id: &str) -> Result<Arc<Hosted>, RpcError> {
        self.agents().get(id).cloned().ok_or_else(|| unknown(id))
    }
}

/// One request outside any conversation.
async fn complete(ready: &Ready, p: params::Complete) -> Result<Value, RpcError> {
    let mut request = CompleteRequest::new(p.prompt);
    request.model = p.model;
    request.system = p.system;
    request.schema = p.schema;
    request.effort = p.effort;
    request.max_tokens = p.max_tokens;
    let done = ready.sdk.complete(request).await.map_err(|e| match e {
        CompleteError::NoProvider { .. } => RpcError::params(e.to_string()),
        other => RpcError::new(rpc::TURN_FAILED, other.to_string()),
    })?;
    Ok(json!({
        "text": done.text, "structured": done.structured, "model": done.model, "usage": done.usage,
        "stopReason": done.stop_reason,
    }))
}

fn command_json(c: &CommandInfo) -> Value {
    let (source, plugin) = match &c.source {
        SkillOrigin::Bundled => ("bundled", None),
        SkillOrigin::UserSkill | SkillOrigin::UserCommand => ("user", None),
        SkillOrigin::ProjectSkill | SkillOrigin::ProjectCommand => ("project", None),
        SkillOrigin::PluginSkill { plugin } | SkillOrigin::PluginCommand { plugin } => ("plugin", Some(plugin)),
    };
    json!({
        "name": c.name, "description": c.description, "argumentHint": c.argument_hint, "source": source,
        "plugin": plugin, "userInvocable": c.user_invocable, "modelInvocable": c.model_invocable,
    })
}

/// A change to a live agent that did not go through.
fn control_failed(e: &ControlError) -> RpcError {
    match e {
        ControlError::Gone(g) => gone(g),
        ControlError::Store(_) | ControlError::Compact(_) | ControlError::NotSummarized => {
            RpcError::new(rpc::TURN_FAILED, e.to_string())
        }
        _ => RpcError::params(e.to_string()),
    }
}

fn mcp_status_json(s: &eos_agent_sdk::McpServerStatus) -> Value {
    let (state, error) = match &s.state {
        eos_agent_sdk::McpServerState::Connected => ("connected", None),
        eos_agent_sdk::McpServerState::Pending => ("pending", None),
        eos_agent_sdk::McpServerState::NeedsAuth => ("needs-auth", None),
        eos_agent_sdk::McpServerState::Idle => ("idle", None),
        eos_agent_sdk::McpServerState::Failed(e) => ("failed", Some(e.as_str())),
    };
    json!({ "name": s.name, "transport": s.transport, "state": state, "tools": s.tools, "error": error })
}

/// Every event of the finished call is passed on before its response:
/// subagents' first (they ran inside the call), then the agent's own.
async fn flush(ready: &Ready, hosted: &Hosted) {
    for pump in [&ready.subagent_flush, &hosted.flush] {
        let (done, wait) = oneshot::channel();
        if pump.send(done).await.is_ok() {
            let _ = wait.await;
        }
    }
}

fn outcome(result: TurnResult) -> Result<Value, RpcError> {
    match result {
        Ok(TurnOutcome::Finished(reason)) => Ok(json!({ "outcome": "finished", "stopReason": reason })),
        Ok(TurnOutcome::Interrupted) => Ok(json!({ "outcome": "interrupted" })),
        Err(e) => Err(failed(&e)),
    }
}

fn info_json(info: &AgentInfo) -> Value {
    json!({
        "agentId": info.agent_id,
        "parentId": info.parent_id,
        "name": info.name,
        "description": info.description,
        "agentType": info.agent_type,
        "model": info.model,
        "busy": info.busy,
        "evicted": info.evicted,
    })
}

fn token_source(auth: &Auth) -> Result<Arc<dyn TokenSource>, RpcError> {
    Ok(match auth {
        Auth::ClaudeStore(dir) => Arc::new(ClaudeLogin::keychain(Some(dir))),
        Auth::ApiKeyEnv(var) => Arc::new(StaticCredential(Credential::ApiKey(env(var)?))),
        Auth::BearerEnv(var) => Arc::new(StaticCredential(Credential::Bearer(env(var)?))),
        Auth::OpenAiStore(dir) => Arc::new(ChatGptLogin::new(dir)),
    })
}

fn env(var: &str) -> Result<String, RpcError> {
    std::env::var(var).map_err(|_| RpcError::params(format!("environment variable {var} is not set")))
}

fn unknown(id: &str) -> RpcError {
    RpcError::new(rpc::UNKNOWN_AGENT, format!("no agent {id}"))
}

fn gone(e: &Gone) -> RpcError {
    RpcError::new(rpc::UNKNOWN_AGENT, e.to_string())
}

fn failed(e: &TurnError) -> RpcError {
    RpcError::new(rpc::TURN_FAILED, e.to_string())
}
