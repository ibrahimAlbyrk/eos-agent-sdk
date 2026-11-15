//! One request to a model outside any conversation: a summary, a title, a
//! judge's verdict. No tools, no transcript, no agent.

use std::sync::Arc;

use eos_agent_agents::{STRUCTURED_OUTPUT, STRUCTURED_OUTPUT_NUDGE, StructuredOutput, validate_output};
use eos_agent_core::{
    Assembled, Assembler, CancellationToken, ModelError, ModelRequest, RequestAuth, RetryPolicy, StoredMessage,
    SystemPrompt, TokenSource, Tool, Toolbox,
};
use eos_agent_types::{ContentBlock, Role, StopReason, StreamEvent, Usage};
use futures_util::StreamExt;
use serde_json::value::RawValue;

use crate::auth::AuthSource;
use crate::models::{self, ThinkingOff};
use crate::profile::Provider;
use crate::sdk::Sdk;
use crate::services::Services;

/// Tries after the first, for a transient failure.
const RETRIES: u32 = 3;

/// What to ask. Only the prompt is required.
#[derive(Debug, Clone)]
pub struct CompleteRequest {
    /// A model id or alias; the SDK's default when `None`.
    pub model: Option<String>,
    /// The credential the request runs on (one for the model's provider), as
    /// [`AgentOptions::auth`](crate::AgentOptions::auth) takes it; the SDK's
    /// own when `None`. A credential that fails ends the request in
    /// [`CompleteError::Model`]: nobody is asked for a replacement.
    pub auth: Option<AuthSource>,
    /// The system prompt. On a Claude model it follows Claude Code's identity
    /// line, as in every request Claude Code makes.
    pub system: Option<String>,
    pub prompt: String,
    /// A JSON Schema the answer must match: the model then answers through
    /// one `StructuredOutput` call, and the result carries its input as
    /// [`Completion::structured`]. The call is forced where the model takes
    /// that; a model that always thinks is asked for it, and asked once more
    /// when it answers without it.
    pub schema: Option<serde_json::Value>,
    /// `low` … `max`, as far as the model takes it; none when `None`.
    pub effort: Option<String>,
    /// The model's output limit (as Claude Code sends it) when `None`.
    pub max_tokens: Option<u32>,
    /// Ends the request with [`CompleteError::Cancelled`] when it fires.
    /// Dropping the future cancels it too.
    pub cancel: Option<CancellationToken>,
}

impl CompleteRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            model: None,
            auth: None,
            system: None,
            prompt: prompt.into(),
            schema: None,
            effort: None,
            max_tokens: None,
            cancel: None,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn auth(mut self, source: Arc<dyn TokenSource>) -> Self {
        self.auth = Some(AuthSource(source));
        self
    }

    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn schema(mut self, schema: serde_json::Value) -> Self {
        self.schema = Some(schema);
        self
    }

    pub fn effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }
}

/// The model's answer.
#[derive(Debug, Clone)]
pub struct Completion {
    /// The answer's text (empty for a structured answer).
    pub text: String,
    /// With a schema: the object the model answered with, as it wrote it,
    /// checked against the schema.
    pub structured: Option<Box<RawValue>>,
    /// What the answering request used (its last try).
    pub usage: Usage,
    /// The model that answered (the alias resolved).
    pub model: String,
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, thiserror::Error)]
pub enum CompleteError {
    /// The model runs on a provider this SDK has no credential for.
    #[error("{model} runs on {}, which has no credential here", provider.name())]
    NoProvider { provider: Provider, model: String },
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("the request was cancelled")]
    Cancelled,
    /// The answer is not what the schema asks for.
    #[error("the answer does not match the schema: {0}")]
    Schema(String),
}

impl Sdk {
    /// Asks a model once, outside any conversation: no tools, no agent,
    /// nothing recorded; thinking is off where the model allows it and
    /// nothing is cached. Transient failures are retried as an agent's
    /// requests are.
    pub async fn complete(&self, request: CompleteRequest) -> Result<Completion, CompleteError> {
        let services = &self.services;
        let model = services.resolve_model(request.model.as_deref().unwrap_or(&services.default_model));
        let effort = request.effort.as_deref().and_then(|e| services.effort_for(&model, Some(e)));
        let off = models::thinking_off(&model, effort);
        let wire = wire_request(services, &model, &request, &off)?;
        let cancel = request.cancel.clone().unwrap_or_default();
        let mut answered = cancellable(services, &wire, &cancel).await?;
        if request.schema.is_some() && off.thinks && !answers_structured(&answered.content) {
            answered = cancellable(services, &nudged(&wire, &answered.content), &cancel).await?;
        }
        let structured = match &request.schema {
            Some(schema) => Some(structured_answer(schema, &answered.content)?),
            None => None,
        };
        let texts: Vec<&str> = answered
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        Ok(Completion {
            text: texts.join("\n"),
            structured,
            usage: answered.usage,
            model,
            stop_reason: answered.stop_reason,
        })
    }
}

fn wire_request(
    services: &Services,
    model: &str,
    request: &CompleteRequest,
    off: &ThinkingOff,
) -> Result<ModelRequest, CompleteError> {
    let no_provider = || CompleteError::NoProvider { provider: Provider::of(model), model: model.to_owned() };
    let profile = services.profile(model).ok_or_else(no_provider)?;
    let identity = (profile.provider == Provider::Anthropic).then_some(&*profile.identity);
    let system: Vec<(&str, _)> = identity.into_iter().chain(request.system.as_deref()).map(|t| (t, None)).collect();
    let (tools, extra) = match &request.schema {
        Some(schema) => {
            let answer: Arc<dyn Tool> = Arc::new(StructuredOutput::new(schema.clone()));
            let choice = if off.thinks {
                r#"{"tool_choice":{"type":"auto"}}"#.to_owned()
            } else {
                format!(r#"{{"tool_choice":{{"type":"tool","name":"{STRUCTURED_OUTPUT}"}}}}"#)
            };
            (Toolbox::new(vec![answer]).json().cloned(), Some(choice.into()))
        }
        None => (None, None),
    };
    let provider = profile.provider.id();
    Ok(ModelRequest {
        model: model.to_owned(),
        auth: request.auth.as_ref().map(|auth| RequestAuth { provider: provider.clone(), source: Arc::clone(&auth.0) }),
        max_tokens: request.max_tokens.unwrap_or_else(|| services.limits(model).0),
        system: SystemPrompt::new(&system).json().clone(),
        tools,
        messages: vec![StoredMessage::user_text(request.prompt.as_str()).json().clone()],
        thinking: off.thinking,
        effort: off.effort.clone(),
        extra,
        cache_key: None,
        betas: &[],
    })
}

async fn cancellable(
    services: &Services,
    request: &ModelRequest,
    cancel: &CancellationToken,
) -> Result<Assembled, CompleteError> {
    tokio::select! {
        answered = send(services, request) => Ok(answered?),
        () = cancel.cancelled() => Err(CompleteError::Cancelled),
    }
}

fn answers_structured(content: &[ContentBlock]) -> bool {
    content.iter().any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == STRUCTURED_OUTPUT))
}

/// `request` once more, after the model's answer without its structured
/// call, with the reminder Claude Code gives a subagent in that case.
fn nudged(request: &ModelRequest, answer: &[ContentBlock]) -> ModelRequest {
    let mut again = request.clone();
    if let Some(reply) = (!answer.is_empty()).then(|| StoredMessage::new(Role::Assistant, answer).ok()).flatten() {
        again.messages.push(reply.json().clone());
    }
    again.messages.push(StoredMessage::user_text(STRUCTURED_OUTPUT_NUDGE).json().clone());
    again
}

/// Runs `request` to its end, again after a transient failure.
async fn send(services: &Services, request: &ModelRequest) -> Result<Assembled, ModelError> {
    let retry = RetryPolicy { max_retries: RETRIES, ..RetryPolicy::default() };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match once(services, request).await {
            Ok(answered) => return Ok(answered),
            Err(e) => e,
        };
        if !error.is_retryable() || attempt > retry.max_retries {
            return Err(error);
        }
        tokio::time::sleep(retry.delay(attempt, error.retry_after())).await;
    }
}

async fn once(services: &Services, request: &ModelRequest) -> Result<Assembled, ModelError> {
    let mut events = services.client.stream(request).await?;
    let mut assembler = Assembler::default();
    while let Some(event) = events.next().await {
        let event = event?;
        if let StreamEvent::Error(e) = &event {
            return Err(ModelError::Api { kind: e.kind.clone(), message: e.message.clone() });
        }
        assembler.push(event);
    }
    let answered = assembler.finish();
    if !answered.complete {
        return Err(ModelError::Transient("stream ended before message_stop".into()));
    }
    Ok(answered)
}

/// The input of the answer's `StructuredOutput` call, when it matches `schema`.
fn structured_answer(schema: &serde_json::Value, content: &[ContentBlock]) -> Result<Box<RawValue>, CompleteError> {
    let input = content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } if name == STRUCTURED_OUTPUT => Some(input),
        _ => None,
    });
    let input = input.ok_or_else(|| CompleteError::Schema("the model gave no structured answer".into()))?;
    let value = serde_json::from_str(input.get()).map_err(|e| CompleteError::Schema(e.to_string()))?;
    let errors = validate_output(schema, &value);
    if errors.is_empty() { Ok(input.to_owned()) } else { Err(CompleteError::Schema(errors.join(", "))) }
}
