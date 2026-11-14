//! Single-shot model calls the tools make outside the agent loop.

use bytes::Bytes;
use eos_agent_core::{
    Assembler, Extensions, ModelClient, ModelError, ModelRequest, RequestAuth, RetryPolicy, StoredMessage,
    SystemPrompt, Thinking,
};
use eos_agent_types::{ContentBlock, StreamEvent};
use futures_util::StreamExt;

/// Settings shared by both tools' side calls.
#[derive(Debug, Clone)]
pub struct SideModel {
    pub model: String,
    pub max_tokens: u32,
    /// First system block of every side call. Claude Code puts its identity
    /// line in front of every request, side calls included; `None` sends
    /// only the tool's own system text.
    pub identity: Option<String>,
    /// The model refuses to turn thinking off: side calls then send no
    /// thinking setting (it thinks as it decides) and force no tool call.
    pub always_thinks: bool,
    pub retry: RetryPolicy,
}

impl SideModel {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tokens: 32_000,
            identity: Some(crate::SDK_IDENTITY.to_owned()),
            always_thinks: false,
            retry: RetryPolicy { max_retries: 3, ..RetryPolicy::default() },
        }
    }

    /// A one-user-message request with thinking off (where the model allows
    /// it) and no prompt caching.
    pub(crate) fn request(&self, system: Option<&str>, prompt: &str) -> ModelRequest {
        let blocks: Vec<(&str, _)> =
            self.identity.as_deref().into_iter().chain(system).map(|text| (text, None)).collect();
        ModelRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            system: SystemPrompt::new(&blocks).json().clone(),
            tools: None,
            messages: vec![StoredMessage::user_text(prompt).json().clone()],
            thinking: (!self.always_thinks).then_some(Thinking::Disabled),
            effort: None,
            extra: None,
            cache_key: None,
            betas: &[],
            auth: None,
        }
    }
}

/// The calling agent's own credential, when it has one: its side requests
/// are billed where its turns are.
pub(crate) fn agent_auth(extensions: &Extensions) -> Option<RequestAuth> {
    extensions.get::<RequestAuth>().map(|auth| (*auth).clone())
}

/// Runs `request` to completion, retrying transient failures.
pub(crate) async fn complete(
    client: &dyn ModelClient,
    request: &ModelRequest,
    retry: &RetryPolicy,
) -> Result<Vec<ContentBlock>, ModelError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match once(client, request).await {
            Ok(content) => return Ok(content),
            Err(e) => e,
        };
        if !error.is_retryable() || attempt > retry.max_retries {
            return Err(error);
        }
        tokio::time::sleep(retry.delay(attempt, error.retry_after())).await;
    }
}

async fn once(client: &dyn ModelClient, request: &ModelRequest) -> Result<Vec<ContentBlock>, ModelError> {
    let mut events = client.stream(request).await?;
    let mut assembler = Assembler::default();
    while let Some(event) = events.next().await {
        let event = event?;
        if let StreamEvent::Error(e) = &event {
            return Err(ModelError::Api { kind: e.kind.clone(), message: e.message.clone() });
        }
        assembler.push(event);
    }
    let done = assembler.finish();
    if !done.complete {
        return Err(ModelError::Transient("stream ended before message_stop".into()));
    }
    Ok(done.content)
}

/// `{"tool_choice":…}`-style extra members, serialized once.
pub(crate) fn json_bytes(value: &serde_json::Value) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("a JSON value serializes"))
}
