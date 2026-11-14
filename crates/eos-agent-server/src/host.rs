//! The client's say over agents: `permission.decide` for every tool call,
//! `user.ask` for AskUserQuestion and `mcp.authorize` for MCP sign-ins, all
//! as requests to the client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall};
use eos_agent_sdk::{AuthPrompter, AuthRequest, StepUpAnswer, StepUpRequest};
use eos_agent_tools_session::{Annotation, Question, Refusal, Reply, UserPrompter};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::rpc::Peer;

/// Claude Code's words for a tool call the user turned down.
const REJECTED: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was \
a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you \
how to proceed.";
const NO_ANSWER: &str =
    "The user did not answer in time. Proceed using your best judgment based on the context so far.";

/// Asks the client about every tool call; no answer in time means no.
#[derive(Debug)]
pub(crate) struct ClientGate {
    pub(crate) peer: Arc<Peer>,
    pub(crate) timeout: Duration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecideParams<'a> {
    agent_id: &'a str,
    tool_use_id: &'a str,
    tool: &'a str,
    input: &'a RawValue,
    sandboxed: bool,
    /// The top-level agent of the caller's tree.
    root_agent_id: &'a str,
    /// Subagents only: who started the caller, and with which tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
    subagent: bool,
    workflow: bool,
    /// The paths the call would create or change, absolute, each inside the
    /// agent's working directories or not.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    writes: Vec<WriteParam<'a>>,
}

#[derive(Serialize)]
struct WriteParam<'a> {
    path: &'a std::path::Path,
    inside: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Decision {
    decision: String,
    #[serde(default)]
    message: Option<String>,
    /// With `allow`: the input the tool runs with instead of the model's.
    #[serde(default)]
    updated_input: Option<Box<RawValue>>,
}

impl PermissionGate for ClientGate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        Box::pin(async move {
            let params = DecideParams {
                agent_id: call.agent_id,
                tool_use_id: call.id,
                tool: call.name,
                input: call.input,
                sandboxed: call.sandboxed,
                root_agent_id: call.root_id(),
                parent_agent_id: call.origin.map(|o| &*o.parent_id),
                parent_tool_use_id: call.origin.and_then(|o| o.tool_use_id.as_deref()),
                subagent: call.origin.is_some(),
                workflow: call.in_workflow(),
                writes: call.writes.iter().map(|w| WriteParam { path: &w.path, inside: w.inside }).collect(),
            };
            let Ok(params) = serde_json::to_string(&params) else { return Permission::Deny(REJECTED.into()) };
            let answer = self.peer.request("permission.decide", &params, self.timeout).await;
            match answer.map(serde_json::from_value::<Decision>) {
                Ok(Ok(Decision { decision, updated_input: Some(input), .. })) if decision == "allow" => {
                    Permission::AllowWith(input.into())
                }
                Ok(Ok(d)) if d.decision == "allow" => Permission::Allow,
                Ok(Ok(d)) => Permission::Deny(d.message.unwrap_or_else(|| REJECTED.into())),
                Ok(Err(e)) => {
                    Permission::Deny(format!("Permission denied: the host's answer was not understood ({e})."))
                }
                Err(why) => Permission::Deny(format!("Permission denied: the host did not decide ({why}).")),
            }
        })
    }
}

/// Puts AskUserQuestion to the client.
#[derive(Debug)]
pub(crate) struct ClientPrompter {
    pub(crate) peer: Arc<Peer>,
    pub(crate) timeout: Duration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AskParams<'a> {
    agent_id: &'a str,
    questions: &'a [Question],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Answer {
    answers: HashMap<String, String>,
    annotations: HashMap<String, Note>,
    response: Option<String>,
    /// Nobody will answer; `reason` goes back to the model.
    declined: bool,
    /// The user turned the questions away (Esc): the turn ends.
    cancelled: bool,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Note {
    preview: Option<String>,
    notes: Option<String>,
}

impl UserPrompter for ClientPrompter {
    fn ask<'a>(
        &'a self,
        agent_id: &'a str,
        _tool_use_id: &'a str,
        questions: &'a [Question],
    ) -> BoxFuture<'a, Result<Reply, Refusal>> {
        Box::pin(async move {
            let params = serde_json::to_string(&AskParams { agent_id, questions }).map_err(|e| e.to_string())?;
            let value = self.peer.request("user.ask", &params, self.timeout).await.map_err(|_| NO_ANSWER.to_owned())?;
            let answer: Answer = serde_json::from_value(value).map_err(|e| format!("Unreadable answer: {e}"))?;
            if answer.cancelled {
                return Err(Refusal::Cancelled);
            }
            if answer.declined {
                return Err(answer.reason.unwrap_or_else(|| NO_ANSWER.into()).into());
            }
            let annotations = answer
                .annotations
                .into_iter()
                .map(|(q, n)| (q, Annotation { preview: n.preview, notes: n.notes }))
                .collect();
            Ok(Reply { answers: answer.answers, annotations, response: answer.response, ..Reply::default() })
        })
    }
}

/// Sends MCP sign-ins to the client (`mcp.authorize`): it opens the address
/// for the user or shows it. The sign-in does not wait for the answer. With
/// `step_up` (permissions `ask`), Claude Code's step-up question goes to the
/// client as `mcp.stepUp` and waits that long for the answer.
#[derive(Debug)]
pub(crate) struct ClientAuthPrompter {
    pub(crate) peer: Arc<Peer>,
    pub(crate) step_up: Option<Duration>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StepUpParams<'a> {
    server: &'a str,
    scope: &'a str,
    question: &'a str,
    choices: Vec<ChoiceParam>,
    agent_id: Option<&'a str>,
}

#[derive(Serialize)]
struct ChoiceParam {
    value: &'static str,
    label: &'static str,
    description: &'static str,
}

#[derive(Deserialize)]
struct StepUpReply {
    choice: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeParams<'a> {
    server: &'a str,
    url: &'a str,
    redirect_uri: &'a str,
    agent_id: Option<&'a str>,
}

impl AuthPrompter for ClientAuthPrompter {
    fn authorize(&self, request: &AuthRequest) {
        let params = AuthorizeParams {
            server: &request.server,
            url: &request.url,
            redirect_uri: &request.redirect_uri,
            agent_id: request.agent_id.as_deref(),
        };
        let Ok(params) = serde_json::to_string(&params) else { return };
        let peer = Arc::clone(&self.peer);
        tokio::spawn(async move {
            let _ = peer.request("mcp.authorize", &params, Duration::from_secs(300)).await;
        });
    }

    fn ask_step_up<'a>(&'a self, request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        Box::pin(async move {
            let Some(timeout) = self.step_up else { return StepUpAnswer::Unanswered };
            let params = StepUpParams {
                server: &request.server,
                scope: &request.scope,
                question: &request.question,
                choices: request
                    .choices
                    .iter()
                    .map(|c| ChoiceParam { value: c.value, label: c.label, description: c.description })
                    .collect(),
                agent_id: request.agent_id.as_deref(),
            };
            let Ok(params) = serde_json::to_string(&params) else { return StepUpAnswer::Unanswered };
            let answer = self.peer.request("mcp.stepUp", &params, timeout).await;
            match answer.ok().and_then(|v| serde_json::from_value::<StepUpReply>(v).ok()).and_then(|r| r.choice) {
                Some(c) if c == "reauthenticate" => StepUpAnswer::Reauthenticate,
                Some(c) if c == "not_now" => StepUpAnswer::NotNow,
                _ => StepUpAnswer::Unanswered,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sign_ins_go_to_the_client_as_mcp_authorize() {
        let (out, mut sent) = tokio::sync::mpsc::channel(4);
        let prompter = ClientAuthPrompter { peer: Arc::new(Peer::new(out)), step_up: None };
        prompter.authorize(&AuthRequest {
            server: "github".into(),
            url: "https://as.dev/authorize?state=x".into(),
            redirect_uri: "http://localhost:3118/callback".into(),
            agent_id: Some("a1".into()),
            step_up: false,
        });
        let line = tokio::time::timeout(Duration::from_secs(5), sent.recv()).await.expect("sent").expect("line");
        let msg: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(msg["method"], "mcp.authorize");
        assert!(msg["id"].is_string());
        assert_eq!(
            msg["params"],
            serde_json::json!({"server": "github", "url": "https://as.dev/authorize?state=x",
                "redirectUri": "http://localhost:3118/callback", "agentId": "a1"})
        );
    }

    #[tokio::test]
    async fn step_up_questions_go_to_the_client_and_wait_for_the_answer() {
        let (out, mut sent) = tokio::sync::mpsc::channel(4);
        let peer = Arc::new(Peer::new(out));
        let prompter = ClientAuthPrompter { peer: Arc::clone(&peer), step_up: Some(Duration::from_secs(5)) };
        let request = StepUpRequest {
            server: "docs".into(),
            scope: "admin".into(),
            question: "MCP server \"docs\" needs additional permissions…".into(),
            choices: eos_agent_sdk::STEP_UP_CHOICES.to_vec(),
            agent_id: Some("a1".into()),
        };
        let answering = async {
            let line = sent.recv().await.expect("line");
            let msg: serde_json::Value = serde_json::from_str(&line).expect("json");
            assert_eq!(msg["method"], "mcp.stepUp");
            assert_eq!(msg["params"]["scope"], "admin");
            assert_eq!(msg["params"]["choices"][0]["value"], "reauthenticate");
            assert_eq!(msg["params"]["agentId"], "a1");
            let id = RawValue::from_string(msg["id"].to_string()).expect("id");
            peer.resolve(&id, Ok(serde_json::json!({"choice": "reauthenticate"})));
        };
        let (answer, ()) = tokio::join!(prompter.ask_step_up(&request), answering);
        assert_eq!(answer, StepUpAnswer::Reauthenticate);
        let silent = ClientAuthPrompter { peer, step_up: None };
        assert_eq!(silent.ask_step_up(&request).await, StepUpAnswer::Unanswered);
    }
}
