//! OpenAI's Responses API as a [`ModelClient`], on a ChatGPT plan through
//! "Sign in with ChatGPT": `POST https://api.openai.com/v1/responses`,
//! `store: false`, `stream: true` (never ChatGPT's `backend-api`).
//!
//! The engine speaks the Messages shapes. Requests are translated on the
//! way out ([`request`]); the stream is translated back into Messages stream
//! events on the way in, so the assembler, transcripts and host protocol do
//! not change. Reasoning travels between turns as encrypted content inside
//! a thinking block's signature.

pub mod auth;
mod errors;
mod limits;
pub mod models;
mod request;
mod stream;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use eos_agent_core::{
    BoxFuture, Credential, EventStream, ModelClient, ModelError, ModelRequest, RateLimitInfo, RequestAuth, TokenSource,
};
use eos_agent_http::{Pieces, SseDecoder};
use futures_util::StreamExt;

pub use auth::{Account, BrowserOpener, ChatGptLogin, LoginError, PrintOpener, SignInConfig, sign_in};
pub use models::{Catalog, ModelCatalog, ModelInfo, is_openai_model};
pub use request::ToolPlacement;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Carries the prompt-cache key so a conversation's requests are routed to
/// the machine holding its cached prefix.
const SESSION_HEADER: &str = "session-id";
/// What a [`RequestAuth`] for this client is labelled.
pub const PROVIDER: &str = "openai";

pub use stream::wrote;
/// How many times one request moves to another credential after its own
/// stopped working.
const MAX_CREDENTIAL_CHANGES: u32 = 8;
/// HTTP/2 pings on every pooled connection, busy or idle, so one that died
/// with a sleep or a network change is dropped instead of reused.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The API root (`…/v1`); requests go to `<base>/responses`.
    pub base_url: String,
    pub user_agent: String,
    /// A stream that sends nothing for this long is treated as dead
    /// (reasoning can be silent for minutes).
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Speak HTTP/2 to a plain-HTTP endpoint without negotiating it (h2c).
    pub http2_prior_knowledge: bool,
    pub tools: ToolPlacement,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            user_agent: concat!("eos-agent/", env!("CARGO_PKG_VERSION")).into(),
            idle_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(30),
            http2_prior_knowledge: false,
            tools: ToolPlacement::default(),
        }
    }
}

/// The stream translation on its own, for benchmarks: each Responses
/// event's data, in order, as Messages events.
#[doc(hidden)]
pub fn translate_stream<'a>(
    events: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<eos_agent_types::StreamEvent>, ModelError> {
    let mut translator = stream::Translator::default();
    let mut out = Vec::new();
    for data in events {
        translator.push(data, &mut out)?;
    }
    Ok(out)
}

/// One client (one connection pool) for every agent on OpenAI models.
pub struct OpenAiClient {
    http: reqwest::Client,
    config: ClientConfig,
    auth: Arc<dyn TokenSource>,
    rate_limit: Mutex<Option<RateLimitInfo>>,
    memo: request::Memo,
}

impl std::fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiClient").field("base_url", &self.config.base_url).finish_non_exhaustive()
    }
}

impl OpenAiClient {
    pub fn new(auth: Arc<dyn TokenSource>, config: ClientConfig) -> Result<Self, ModelError> {
        let mut http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
            .http2_keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true);
        if config.http2_prior_knowledge {
            http = http.http2_prior_knowledge();
        }
        let http = http.build().map_err(|e| ModelError::Invalid(format!("HTTP client: {e}")))?;
        Ok(Self { http, config, auth, rate_limit: Mutex::new(None), memo: request::Memo::default() })
    }

    /// The plan-limit state from the most recent response that reported one.
    pub fn rate_limit(&self) -> Option<RateLimitInfo> {
        self.rate_limit.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The request body `request` becomes, as pieces (for tests and benchmarks).
    pub fn body(&self, request: &ModelRequest) -> Result<Vec<Bytes>, ModelError> {
        request::build(request, request::Shape { tools: self.config.tools }, &self.memo)
    }

    /// `GET <base>/models`: the signed-in account's catalog, as sent.
    pub async fn list_models(&self) -> Result<serde_json::Value, ModelError> {
        let url = format!("{}/models", self.config.base_url);
        let mut cred = self.auth.credential().await?;
        let res = self.authorized(&*self.auth, &mut cred, |cred| self.http.get(&url).bearer_auth(token(cred))).await?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| ModelError::Transient(e.to_string()))?;
        if status != 200 {
            return Err(ModelError::Invalid(format!(
                "listing models: HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        serde_json::from_str(&text).map_err(|e| ModelError::Invalid(format!("listing models: {e}")))
    }

    /// Sends with `cred` (from `auth`); a 401 renews it once and retries,
    /// leaving the credential last sent in `cred`.
    async fn authorized(
        &self,
        auth: &dyn TokenSource,
        cred: &mut Credential,
        build: impl Fn(&Credential) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ModelError> {
        let mut renewed = false;
        loop {
            // A connection that died unnoticed never answers: waiting for the
            // response to start is bounded like a silent stream.
            let send = build(cred).header("user-agent", &self.config.user_agent).send();
            let res = tokio::time::timeout(self.config.idle_timeout, send)
                .await
                .map_err(|_| ModelError::Stalled)?
                .map_err(|e| ModelError::Transient(e.to_string()))?;
            if res.status().as_u16() == 401 && !renewed && auth.refused(cred).await {
                renewed = true;
                *cred = auth.credential().await?;
                continue;
            }
            return Ok(res);
        }
    }

    async fn open(&self, request: &ModelRequest) -> Result<EventStream, ModelError> {
        let body = self.body(request)?;
        let url = format!("{}/responses", self.config.base_url);
        let auth = RequestAuth::choose(request.auth.as_ref(), PROVIDER, &self.auth);
        let mut cred = auth.credential().await?;
        let mut changes = 0;
        loop {
            let res = self
                .authorized(auth, &mut cred, |cred| {
                    let mut req = self
                        .http
                        .post(&url)
                        .bearer_auth(token(cred))
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream");
                    // `prompt_cache_key` alone does not reach the cache on a
                    // ChatGPT plan: requests without a session header land on
                    // machines that never saw the prefix (0 cached tokens, measured
                    // live). Codex sends the same header.
                    if let Some(key) = &request.cache_key {
                        req = req.header(SESSION_HEADER, key.as_ref());
                    }
                    req.body(reqwest::Body::wrap(Pieces::new(body.clone())))
                })
                .await?;
            let info = limits::from_headers(res.headers());
            if let Some(info) = &info {
                auth.observe(info);
                *self.rate_limit.lock().unwrap_or_else(PoisonError::into_inner) = Some(info.clone());
            }
            let status = res.status().as_u16();
            if status == 200 {
                return Ok(events(res, self.config.idle_timeout));
            }
            let headers = res.headers().clone();
            let text = tokio::time::timeout(self.config.idle_timeout, res.text())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            let error = with_request_id(errors::from_status(status, &headers, &text, info), &headers);
            // The credential is used up or signed out: the source may have another.
            let unusable = matches!(error, ModelError::LimitReached { .. } | ModelError::Auth(_));
            if !unusable || changes == MAX_CREDENTIAL_CHANGES || !auth.failed(&cred, &error).await {
                return Err(error);
            }
            changes += 1;
            cred = auth.credential().await?;
        }
    }
}

fn token(cred: &Credential) -> &str {
    match cred {
        Credential::ApiKey(t) | Credential::Bearer(t) => t,
    }
}

/// Keeps the request id with a failure, as OpenAI asks.
fn with_request_id(error: ModelError, headers: &reqwest::header::HeaderMap) -> ModelError {
    let Some(id) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) else { return error };
    match error {
        ModelError::Auth(m) => ModelError::Auth(format!("{m} (request {id})")),
        ModelError::Invalid(m) => ModelError::Invalid(format!("{m} (request {id})")),
        other => other,
    }
}

impl ModelClient for OpenAiClient {
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
                    continue;
                }
                if let Err(e) = s.translator.push(&frame.data, &mut s.ready) {
                    s.done = true;
                    return Some((Err(e), s));
                }
                // Handed out by popping: oldest last.
                s.ready.reverse();
                if s.translator.finished() {
                    s.done = true;
                }
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
                Ok(None) => {
                    s.done = true;
                    return Some((Err(ModelError::Transient("stream ended before response.completed".into())), s));
                }
            }
        }
    })
    .boxed()
}
