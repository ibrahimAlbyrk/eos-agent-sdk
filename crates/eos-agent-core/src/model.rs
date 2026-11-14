use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use eos_agent_types::StreamEvent;
use futures_util::stream::BoxStream;

use crate::BoxFuture;
use crate::auth::RequestAuth;

/// Everything one Messages request needs. The large parts (system, tools,
/// messages) are pre-serialized bytes, so building a request copies nothing.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub max_tokens: u32,
    /// The `system` array.
    pub system: Bytes,
    /// The `tools` array (`None` = no tools).
    pub tools: Option<Bytes>,
    /// Each element is one message object.
    pub messages: Vec<Bytes>,
    pub thinking: Option<Thinking>,
    /// `output_config.effort` (`low`, `medium`, `high`, `xhigh`, `max`).
    pub effort: Option<String>,
    /// Extra top-level members, as a JSON object (`{"metadata":…}`); merged as-is.
    pub extra: Option<Bytes>,
    /// The prompt-cache key (the agent's id; a fork keeps its parent's), for
    /// providers whose prompt cache is routed by a key instead of by markers.
    pub cache_key: Option<Arc<str>>,
    /// Betas this request needs besides the client's own (the
    /// `anthropic-beta` header).
    pub betas: &'static [&'static str],
    /// The credential to send it on (the agent's own); the client's when `None`.
    pub auth: Option<RequestAuth>,
}

impl ModelRequest {
    /// Adds a user message at the end of the conversation, ahead of the
    /// `role: "system"` turns that close it: the API takes a system turn
    /// only right before the model's reply, or last.
    pub fn push_user(&mut self, message: Bytes) {
        let at = self.messages.iter().rposition(|m| !m.starts_with(br#"{"role":"system""#)).map_or(0, |i| i + 1);
        self.messages.insert(at, message);
    }
}

/// An agent's latest model request while its turn runs, kept in its
/// [`Extensions`](crate::Extensions) when the host put one there: what a
/// side question is answered from without waiting for the turn.
#[derive(Debug, Default)]
pub struct LiveRequest(Mutex<Option<ModelRequest>>);

impl LiveRequest {
    pub fn set(&self, request: Option<ModelRequest>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = request;
    }

    pub fn get(&self) -> Option<ModelRequest> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// The model decides; `summarized` streams a readable summary, `omitted` does not.
    Adaptive {
        summarized: bool,
    },
    /// A fixed thinking budget, for models without adaptive thinking.
    Enabled {
        budget_tokens: u32,
        summarized: bool,
    },
    Disabled,
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, ModelError>>;

/// The model provider. Adapters map their transport and errors onto this.
pub trait ModelClient: Send + Sync {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>>;
}

/// Plan-limit state as the API reports it on every response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimitInfo {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: Option<String>,
    /// Unix seconds when the binding window resets.
    pub resets_at: Option<u64>,
    /// Which window binds: `five_hour`, `seven_day`, ….
    pub claim: Option<String>,
    pub five_hour_utilization: Option<f64>,
    pub seven_day_utilization: Option<f64>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    /// The plan's limit is used up; retrying before `resets_at` is pointless.
    #[error("usage limit reached")]
    LimitReached { info: RateLimitInfo },
    #[error("overloaded")]
    Overloaded { retry_after: Option<Duration> },
    #[error("transient failure: {0}")]
    Transient(String),
    #[error("stream stalled")]
    Stalled,
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("request rejected: {0}")]
    Invalid(String),
    /// The conversation no longer fits the model's context window.
    #[error("the conversation is too long for the model's context window: {0}")]
    ContextWindowExceeded(String),
    #[error("the API reported an error: {kind}: {message}")]
    Api { kind: String, message: String },
}

impl ModelError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Overloaded { .. } | Self::Transient(_) | Self::Stalled => true,
            Self::Api { kind, .. } => kind == "overloaded_error" || kind == "api_error",
            Self::LimitReached { .. } | Self::Auth(_) | Self::Invalid(_) | Self::ContextWindowExceeded(_) => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Overloaded { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::StoredMessage;

    fn roles(request: &ModelRequest) -> Vec<String> {
        let role = |m: &Bytes| serde_json::from_slice::<serde_json::Value>(m).expect("json")["role"].to_string();
        request.messages.iter().map(|m| role(m).trim_matches('"').to_owned()).collect()
    }

    #[test]
    fn a_user_message_joins_ahead_of_the_closing_system_turns() {
        let mut request = ModelRequest {
            model: "m".into(),
            max_tokens: 1,
            system: Bytes::from_static(b"[]"),
            tools: None,
            messages: vec![
                StoredMessage::user_text("hi").json().clone(),
                StoredMessage::system_text("# Environment").json().clone(),
            ],
            thinking: None,
            effort: None,
            extra: None,
            cache_key: None,
            betas: &[],
            auth: None,
        };
        request.push_user(StoredMessage::user_text("by the way?").json().clone());
        assert_eq!(roles(&request), ["user", "user", "system"]);
        request.messages.push(StoredMessage::user_text("last").json().clone());
        request.push_user(StoredMessage::user_text("after").json().clone());
        assert_eq!(roles(&request), ["user", "user", "system", "user", "user"]);
    }
}
