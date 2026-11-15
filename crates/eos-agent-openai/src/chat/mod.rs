//! The Chat Completions API (`POST <base>/chat/completions`) as a
//! [`ModelClient`]: what OpenAI-compatible servers speak (OpenRouter, Groq,
//! DeepSeek, Gemini's compatible endpoint, Ollama, LM Studio, …). Requests
//! and streams are translated to and from the Messages shapes like the
//! Responses client's; what differs between servers is a [`Compat`].

mod request;
mod stream;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use eos_agent_core::{
    BoxFuture, Credential, EventStream, ModelClient, ModelError, ModelRequest, RequestAuth, TokenSource,
};
use eos_agent_http::SseDecoder;
use futures_util::StreamExt;

const CHAT_PATH: &str = "/chat/completions";

/// The request member that limits the reply's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxTokens {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
    /// The server takes neither.
    Omit,
}

/// Where a server differs from the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compat {
    pub max_tokens: MaxTokens,
    /// Ask for token usage with the stream (`stream_options.include_usage`).
    pub stream_usage: bool,
    /// Send the effort as `reasoning_effort`.
    pub effort: bool,
    /// Send a reply's reasoning back with it (`reasoning_content`).
    pub replay_reasoning: bool,
    /// The models read images.
    pub images: bool,
}

impl Default for Compat {
    fn default() -> Self {
        Self {
            max_tokens: MaxTokens::default(),
            stream_usage: true,
            effort: false,
            replay_reasoning: false,
            images: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatConfig {
    /// What the provider is called here: the `<provider>/` a model id opens
    /// with, and what a [`RequestAuth`] for this client is labelled.
    pub provider: String,
    /// The API root (`…/v1`).
    pub base_url: String,
    /// Where chat completions are under the root; the model list is beside it.
    pub chat_path: String,
    /// The header the key goes in (`None`: `Authorization: Bearer`).
    pub key_header: Option<String>,
    /// Sent with every request.
    pub headers: Vec<(String, String)>,
    pub compat: Compat,
    pub user_agent: String,
    /// A stream that sends nothing for this long is treated as dead.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
}

impl ChatConfig {
    pub fn new(provider: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            chat_path: CHAT_PATH.into(),
            key_header: None,
            headers: Vec::new(),
            compat: Compat::default(),
            user_agent: concat!("eos-agent/", env!("CARGO_PKG_VERSION")).into(),
            idle_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(30),
        }
    }
}

/// One client (one connection pool) for every agent on one provider.
pub struct ChatClient {
    http: reqwest::Client,
    config: ChatConfig,
    auth: Arc<dyn TokenSource>,
}

impl std::fmt::Debug for ChatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatClient").field("provider", &self.config.provider).finish_non_exhaustive()
    }
}

impl ChatClient {
    pub fn new(auth: Arc<dyn TokenSource>, config: ChatConfig) -> Result<Self, ModelError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| ModelError::Invalid(format!("HTTP client: {e}")))?;
        Ok(Self { http, config, auth })
    }

    /// `model` as the provider names it: without the `<provider>/` it opens with here.
    fn model<'a>(&self, model: &'a str) -> &'a str {
        model.strip_prefix(self.config.provider.as_str()).and_then(|rest| rest.strip_prefix('/')).unwrap_or(model)
    }

    /// The request body `request` becomes (for tests).
    pub fn body(&self, request: &ModelRequest) -> Result<Vec<u8>, ModelError> {
        request::build(request, self.model(&request.model), &self.config.compat)
    }

    fn authorized(&self, mut req: reqwest::RequestBuilder, cred: &Credential) -> reqwest::RequestBuilder {
        let (Credential::ApiKey(key) | Credential::Bearer(key)) = cred;
        // A server that asks for no key (a local one) gets none.
        if !key.is_empty() {
            req = match &self.config.key_header {
                Some(header) => req.header(header.as_str(), key),
                None => req.bearer_auth(key),
            };
        }
        for (name, value) in &self.config.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req.header("user-agent", &self.config.user_agent)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, ModelError> {
        // A connection that died unnoticed never answers.
        tokio::time::timeout(self.config.idle_timeout, req.send())
            .await
            .map_err(|_| ModelError::Stalled)?
            .map_err(|e| ModelError::Transient(e.to_string()))
    }

    /// `GET <base>/models`: the ids of the models the provider lists.
    pub async fn list_models(&self) -> Result<Vec<String>, ModelError> {
        let cred = self.auth.credential().await?;
        // Beside the chat path: `…/v1/chat/completions` → `…/v1/models`.
        let beside = self.config.chat_path.strip_suffix(CHAT_PATH).unwrap_or_default();
        let url = format!("{}{beside}/models", self.config.base_url);
        let res = self.send(self.authorized(self.http.get(url), &cred)).await?;
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let text = res.text().await.map_err(|e| ModelError::Transient(e.to_string()))?;
        if status != 200 {
            return Err(crate::errors::from_status(status, &headers, &text, None));
        }
        let doc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ModelError::Invalid(format!("listing models: {e}")))?;
        let list = doc["data"].as_array().or_else(|| doc["models"].as_array()).cloned().unwrap_or_default();
        let id = |m: &serde_json::Value| m["id"].as_str().or_else(|| m["name"].as_str()).map(str::to_owned);
        Ok(list.iter().filter_map(id).collect())
    }

    async fn open(&self, request: &ModelRequest) -> Result<EventStream, ModelError> {
        let body = Bytes::from(self.body(request)?);
        let url = format!("{}{}", self.config.base_url, self.config.chat_path);
        let auth = RequestAuth::choose(request.auth.as_ref(), &self.config.provider, &self.auth);
        let mut cred = auth.credential().await?;
        let mut renewed = false;
        loop {
            let req = self
                .authorized(self.http.post(&url), &cred)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(body.clone());
            let res = self.send(req).await?;
            let status = res.status().as_u16();
            if status == 200 {
                return Ok(events(res, self.config.idle_timeout));
            }
            if status == 401 && !renewed && auth.refused(&cred).await {
                renewed = true;
                cred = auth.credential().await?;
                continue;
            }
            let headers = res.headers().clone();
            let text = tokio::time::timeout(self.config.idle_timeout, res.text())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            return Err(too_long(crate::errors::from_status(status, &headers, &text, None)));
        }
    }
}

/// A refusal that says the conversation does not fit, in a server's own words.
fn too_long(error: ModelError) -> ModelError {
    let ModelError::Invalid(message) = &error else { return error };
    let lower = message.to_lowercase();
    let says = ["context length", "context window", "maximum context", "too many tokens", "prompt is too long"];
    if says.iter().any(|s| lower.contains(s)) { ModelError::ContextWindowExceeded(message.clone()) } else { error }
}

impl ModelClient for ChatClient {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        Box::pin(self.open(request))
    }
}

/// The response body as Messages events, with an idle watchdog.
fn events(res: reqwest::Response, idle: Duration) -> EventStream {
    struct State {
        bytes: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
        decoder: SseDecoder,
        translator: stream::Translator,
        /// Translated events not handed out yet, oldest last.
        ready: Vec<eos_agent_types::StreamEvent>,
        done: bool,
    }
    let state = State {
        bytes: res.bytes_stream().boxed(),
        decoder: SseDecoder::default(),
        translator: stream::Translator::default(),
        ready: Vec::new(),
        done: false,
    };
    futures_util::stream::unfold(state, move |mut s| async move {
        loop {
            if let Some(event) = s.ready.pop() {
                return Some((Ok(event), s));
            }
            if s.done {
                return None;
            }
            if let Some(frame) = s.decoder.next_frame() {
                if frame.data == "[DONE]" {
                    s.translator.finish(&mut s.ready);
                } else if let Err(e) = s.translator.push(&frame.data, &mut s.ready) {
                    s.done = true;
                    return Some((Err(e), s));
                }
                // Handed out by popping: oldest last.
                s.ready.reverse();
                s.done = s.translator.finished();
                continue;
            }
            match tokio::time::timeout(idle, s.bytes.next()).await {
                Err(_) => {
                    s.done = true;
                    return Some((Err(ModelError::Stalled), s));
                }
                Ok(Some(Ok(chunk))) => s.decoder.push(&chunk),
                Ok(Some(Err(e))) => {
                    s.done = true;
                    return Some((Err(ModelError::Transient(format!("stream broke: {e}"))), s));
                }
                // Some servers end the stream without `[DONE]`.
                Ok(None) if s.translator.has_finish() => {
                    s.translator.finish(&mut s.ready);
                    s.ready.reverse();
                    s.done = true;
                }
                Ok(None) => {
                    s.done = true;
                    return Some((Err(ModelError::Transient("stream ended before the reply did".into())), s));
                }
            }
        }
    })
    .boxed()
}
