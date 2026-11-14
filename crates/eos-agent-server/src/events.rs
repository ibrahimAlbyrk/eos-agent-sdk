//! `agent.event` notifications: every [`AgentEvent`] in a stable JSON shape
//! (documented in `docs/protocol.md`), and the tasks that pass them on.

use std::sync::Arc;

use eos_agent_core::{AgentEvent, BlockKind};
use eos_agent_sdk::{SubagentEvent, SubagentEventKind, TaskEvent, TaskEventKind, TaskUsage};
use eos_agent_types::{StopReason, ToolResultContent, Usage};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot};

use crate::rpc::Peer;

/// A request to pass on everything queued so far, then say so.
pub(crate) type Flush = oneshot::Sender<()>;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Wire<'a> {
    #[serde(rename_all = "camelCase")]
    RequestStarted {
        attempt: u32,
    },
    StreamRestarted,
    TextDelta {
        text: &'a str,
    },
    ThinkingDelta {
        text: &'a str,
    },
    #[serde(rename_all = "camelCase")]
    ToolInputDelta {
        tool_use_id: &'a str,
        partial_json: &'a str,
    },
    #[serde(rename_all = "camelCase")]
    ToolUse {
        tool_use_id: &'a str,
        name: &'a str,
        input: &'a RawValue,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: &'a str,
        content: &'a ToolResultContent,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<&'a RawValue>,
    },
    /// The message exactly as the API receives it: `{"role":…,"content":[…]}`.
    Message {
        message: &'a RawValue,
    },
    Usage {
        usage: &'a Usage,
        model: &'a str,
    },
    BlockStart {
        index: u32,
        /// `text`, `thinking`, `tool_use`, `server_tool_use` or the block's wire type.
        block: &'a str,
        #[serde(rename = "toolUseId", skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
    },
    BlockStop {
        index: u32,
    },
    #[serde(rename_all = "camelCase")]
    Compacting {
        before_tokens: u64,
    },
    #[serde(rename_all = "camelCase")]
    RateLimit {
        status: Option<&'a str>,
        resets_at: Option<u64>,
        claim: Option<&'a str>,
        five_hour_utilization: Option<f64>,
        seven_day_utilization: Option<f64>,
    },
    #[serde(rename_all = "camelCase")]
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: &'a str,
    },
    #[serde(rename_all = "camelCase")]
    TurnEnded {
        stop_reason: &'a StopReason,
    },
    Interrupted,
    #[serde(rename_all = "camelCase")]
    Compacted {
        before_tokens: u64,
    },
}

/// The event as JSON; `None` only if a message held invalid JSON.
pub(crate) fn event_json(event: &AgentEvent) -> Option<String> {
    let wire = match event {
        AgentEvent::RequestStarted { attempt } => Wire::RequestStarted { attempt: *attempt },
        AgentEvent::StreamRestarted => Wire::StreamRestarted,
        AgentEvent::TextDelta { text } => Wire::TextDelta { text },
        AgentEvent::ThinkingDelta { text } => Wire::ThinkingDelta { text },
        AgentEvent::ToolInputDelta { id, partial_json } => Wire::ToolInputDelta { tool_use_id: id, partial_json },
        AgentEvent::ToolUse { id, name, input } => Wire::ToolUse { tool_use_id: id, name, input },
        AgentEvent::ToolResult { id, output } => Wire::ToolResult {
            tool_use_id: id,
            content: &output.content,
            is_error: output.is_error,
            details: output.details.as_deref(),
        },
        AgentEvent::Message(message) => Wire::Message { message: serde_json::from_slice(message.json()).ok()? },
        AgentEvent::Usage(usage) => Wire::Usage { usage: &usage.usage, model: &usage.model },
        AgentEvent::BlockStart { index, block } => {
            let (block, tool_use_id, name) = match block {
                BlockKind::Text => ("text", None, None),
                BlockKind::Thinking => ("thinking", None, None),
                BlockKind::ToolUse { id, name } => ("tool_use", Some(id.as_str()), Some(name.as_str())),
                BlockKind::ServerToolUse { id, name } => ("server_tool_use", Some(id.as_str()), Some(name.as_str())),
                BlockKind::Other(kind) => (kind.as_str(), None, None),
            };
            Wire::BlockStart { index: *index, block, tool_use_id, name }
        }
        AgentEvent::BlockStop { index } => Wire::BlockStop { index: *index },
        AgentEvent::Compacting { before_tokens } => Wire::Compacting { before_tokens: *before_tokens },
        AgentEvent::RateLimit(info) => Wire::RateLimit {
            status: info.status.as_deref(),
            resets_at: info.resets_at,
            claim: info.claim.as_deref(),
            five_hour_utilization: info.five_hour_utilization,
            seven_day_utilization: info.seven_day_utilization,
        },
        AgentEvent::Retrying { attempt, delay, error } => {
            Wire::Retrying { attempt: *attempt, delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX), error }
        }
        AgentEvent::TurnEnded { stop_reason, .. } => Wire::TurnEnded { stop_reason },
        AgentEvent::Interrupted => Wire::Interrupted,
        AgentEvent::Compacted { before_tokens } => Wire::Compacted { before_tokens: *before_tokens },
    };
    serde_json::to_string(&wire).ok()
}

/// A subagent turn's start or end as an `agent.event` of that subagent.
fn subagent_json(life: &SubagentEventKind) -> Option<String> {
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum Life<'a> {
        #[serde(rename_all = "camelCase")]
        SubagentStarted {
            tool_use_id: &'a str,
            agent_type: &'a str,
            model: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            effort: Option<&'a str>,
            description: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
            background: bool,
        },
        #[serde(rename_all = "camelCase")]
        SubagentEnded {
            status: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<&'a str>,
            usage: &'a Usage,
            tool_uses: u32,
            duration_ms: u64,
        },
    }
    let life = match life {
        SubagentEventKind::Started(about) => Life::SubagentStarted {
            tool_use_id: &about.tool_use_id,
            agent_type: &about.agent_type,
            model: &about.model,
            effort: about.effort.as_deref(),
            description: &about.description,
            name: about.name.as_deref(),
            background: about.background,
        },
        SubagentEventKind::Ended(end) => Life::SubagentEnded {
            status: end.status,
            error: end.error.as_deref(),
            usage: &end.usage,
            tool_uses: end.tool_uses,
            duration_ms: end.duration_ms,
        },
        SubagentEventKind::Agent(_) => return None,
    };
    serde_json::to_string(&life).ok()
}

async fn emit(peer: &Peer, agent_id: &str, parent_id: Option<&str>, event: &AgentEvent) {
    let Some(json) = event_json(event) else { return };
    notify_event(peer, agent_id, parent_id, &json).await;
}

async fn notify_event(peer: &Peer, agent_id: &str, parent_id: Option<&str>, json: &str) {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        agent_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<&'a str>,
        event: &'a RawValue,
    }
    let Ok(event) = serde_json::from_str::<&RawValue>(json) else { return };
    if let Ok(params) = serde_json::to_string(&Params { agent_id, parent_id, event }) {
        peer.notify("agent.event", &params).await;
    }
}

/// Passes a top-level agent's events on until the agent ends. A flush
/// request first passes on everything already queued.
pub(crate) async fn pump_agent(
    peer: Arc<Peer>,
    agent_id: String,
    mut events: mpsc::Receiver<AgentEvent>,
    mut flushes: mpsc::Receiver<Flush>,
) {
    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Some(event) => emit(&peer, &agent_id, None, &event).await,
                None => break,
            },
            Some(done) = flushes.recv() => {
                while let Ok(event) = events.try_recv() {
                    emit(&peer, &agent_id, None, &event).await;
                }
                let _ = done.send(());
            }
        }
    }
    // Late flush requests find the agent gone; answer them all the same.
    flushes.close();
    while let Some(done) = flushes.recv().await {
        let _ = done.send(());
    }
}

/// Passes every subagent's events on, tagged with the agent that started it.
pub(crate) async fn pump_subagents(
    peer: Arc<Peer>,
    mut events: mpsc::Receiver<SubagentEvent>,
    mut flushes: mpsc::Receiver<Flush>,
) {
    let send = async |e: SubagentEvent| match &e.event {
        SubagentEventKind::Agent(event) => emit(&peer, &e.agent_id, Some(&e.parent_id), event).await,
        life => {
            if let Some(json) = subagent_json(life) {
                notify_event(&peer, &e.agent_id, Some(&e.parent_id), &json).await;
            }
        }
    };
    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Some(event) => send(event).await,
                None => break,
            },
            Some(done) = flushes.recv() => {
                while let Ok(event) = events.try_recv() {
                    send(event).await;
                }
                let _ = done.send(());
            }
        }
    }
}

/// `task.event`: a background task's start, progress and end (Claude Code's
/// SDK `task_started` / `task_progress` / `task_notification`).
pub(crate) fn task_json(event: &TaskEvent) -> Option<String> {
    #[derive(Serialize)]
    struct Usage {
        total_tokens: u64,
        tool_uses: u64,
        duration_ms: u64,
    }
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum Kind<'a> {
        #[serde(rename_all = "camelCase")]
        Started {
            task_type: &'a str,
            description: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            workflow_name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            run_id: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            transcript_dir: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            command: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output_file: Option<&'a str>,
        },
        #[serde(rename_all = "camelCase")]
        Output {
            total_bytes: u64,
        },
        Event {
            text: &'a str,
        },
        #[serde(rename_all = "camelCase")]
        Progress {
            description: &'a str,
            usage: Usage,
            #[serde(skip_serializing_if = "Option::is_none")]
            last_tool_name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            summary: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            workflow_progress: Option<&'a RawValue>,
        },
        #[serde(rename_all = "camelCase")]
        Ended {
            status: &'a str,
            summary: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            output_file: Option<&'a str>,
            usage: Usage,
            #[serde(skip_serializing_if = "Option::is_none")]
            workflow_progress: Option<&'a RawValue>,
            #[serde(skip_serializing_if = "Option::is_none")]
            exit_code: Option<i32>,
        },
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Params<'a> {
        agent_id: &'a str,
        task_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<&'a str>,
        event: Kind<'a>,
    }
    let usage =
        |u: &TaskUsage| Usage { total_tokens: u.total_tokens, tool_uses: u.tool_uses, duration_ms: u.duration_ms };
    let kind = match &event.kind {
        TaskEventKind::Started {
            task_type,
            description,
            workflow_name,
            run_id,
            transcript_dir,
            command,
            output_file,
        } => Kind::Started {
            task_type,
            description,
            workflow_name: workflow_name.as_deref(),
            run_id: run_id.as_deref(),
            transcript_dir: transcript_dir.as_deref(),
            command: command.as_deref(),
            output_file: output_file.as_deref(),
        },
        TaskEventKind::Output { total_bytes } => Kind::Output { total_bytes: *total_bytes },
        TaskEventKind::Event { text } => Kind::Event { text },
        TaskEventKind::Progress { description, usage: u, last_tool_name, summary, workflow_progress } => {
            Kind::Progress {
                description,
                usage: usage(u),
                last_tool_name: last_tool_name.as_deref(),
                summary: summary.as_deref(),
                workflow_progress: workflow_progress.as_deref(),
            }
        }
        TaskEventKind::Ended { status, summary, output_file, usage: u, workflow_progress, exit_code } => Kind::Ended {
            status,
            summary,
            output_file: output_file.as_deref(),
            usage: usage(u),
            workflow_progress: workflow_progress.as_deref(),
            exit_code: *exit_code,
        },
    };
    serde_json::to_string(&Params {
        agent_id: &event.agent_id,
        task_id: &event.task_id,
        tool_use_id: event.tool_use_id.as_deref(),
        event: kind,
    })
    .ok()
}

/// Passes background task events on as `task.event`.
pub(crate) async fn pump_tasks(peer: Arc<Peer>, mut events: mpsc::Receiver<TaskEvent>) {
    while let Some(event) = events.recv().await {
        if let Some(json) = task_json(&event) {
            peer.notify("task.event", &json).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use eos_agent_core::{StoredMessage, ToolOutput};

    use super::*;

    fn json(event: AgentEvent) -> serde_json::Value {
        serde_json::from_str(&event_json(&event).expect("json")).expect("valid")
    }

    #[test]
    fn shapes_are_stable() {
        assert_eq!(
            json(AgentEvent::TextDelta { text: "hi".into() }),
            serde_json::json!({"type":"textDelta","text":"hi"})
        );
        let input = RawValue::from_string(r#"{"file_path":"/a"}"#.into()).expect("raw");
        let tool_use = json(AgentEvent::ToolUse { id: "t1".into(), name: "Read".into(), input: Arc::from(input) });
        assert_eq!(
            tool_use,
            serde_json::json!({"type":"toolUse","toolUseId":"t1","name":"Read","input":{"file_path":"/a"}})
        );
        let result = json(AgentEvent::ToolResult { id: "t1".into(), output: Arc::new(ToolOutput::error("nope")) });
        assert_eq!(result, serde_json::json!({"type":"toolResult","toolUseId":"t1","content":"nope","isError":true}));
        let message = json(AgentEvent::Message(StoredMessage::user_text("q")));
        assert_eq!(message["message"], serde_json::json!({"role":"user","content":[{"type":"text","text":"q"}]}));
        let retry = json(AgentEvent::Retrying { attempt: 2, delay: Duration::from_millis(1500), error: "x".into() });
        assert_eq!(retry, serde_json::json!({"type":"retrying","attempt":2,"delayMs":1500,"error":"x"}));
        let start =
            AgentEvent::BlockStart { index: 1, block: BlockKind::ToolUse { id: "t1".into(), name: "Read".into() } };
        assert_eq!(
            json(start),
            serde_json::json!({"type":"blockStart","index":1,"block":"tool_use","toolUseId":"t1","name":"Read"})
        );
        assert_eq!(
            json(AgentEvent::BlockStart { index: 0, block: BlockKind::Text }),
            serde_json::json!({"type":"blockStart","index":0,"block":"text"})
        );
        let ended = json(AgentEvent::TurnEnded { stop_reason: StopReason::EndTurn, stated: true });
        assert_eq!(ended, serde_json::json!({"type":"turnEnded","stopReason":"end_turn"}));
    }

    #[test]
    fn task_events_keep_claude_codes_rows() {
        let text = r#"[{"type":"workflow_agent","index":1,"label":"a","state":"done"}]"#;
        let rows: serde_json::Value = serde_json::from_str(text).expect("rows");
        let event = TaskEvent {
            agent_id: "main".into(),
            task_id: "w1".into(),
            tool_use_id: Some("toolu_1".into()),
            kind: TaskEventKind::Progress {
                description: "Find: a".into(),
                usage: TaskUsage { total_tokens: 5, tool_uses: 1, duration_ms: 9 },
                last_tool_name: Some("a".into()),
                summary: None,
                workflow_progress: Some(Arc::from(RawValue::from_string(text.to_owned()).expect("raw"))),
            },
        };
        let json: serde_json::Value = serde_json::from_str(&task_json(&event).expect("json")).expect("valid");
        assert_eq!(
            json,
            serde_json::json!({"agentId": "main", "taskId": "w1", "toolUseId": "toolu_1", "event": {"type": "progress",
                "description": "Find: a", "usage": {"total_tokens": 5, "tool_uses": 1, "duration_ms": 9},
                "lastToolName": "a", "workflowProgress": rows}})
        );
    }
}
