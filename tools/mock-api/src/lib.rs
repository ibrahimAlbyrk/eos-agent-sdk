//! A local stand-in for `POST /v1/messages` and OpenAI's `POST /v1/responses`
//! (with `GET /v1/models`). Both routes take their replies from one script,
//! so a test can follow a conversation that crosses providers.
//!
//! A test queues [`Reply`]s; each request takes the next one. Replies can be
//! normal streams, HTTP errors (429/529 with headers), slow streams, or streams
//! that break mid-way, so every retry and recovery path is reproducible.
//! A `/v1/messages` request whose tool calls and results are not paired is
//! refused with the API's 400 (see [`pairing_violation`]).
//! For load tests, [`MockApi::responder`] answers every request from its
//! body instead, and records nothing.

mod chat;
mod pairing;
mod reply;
mod responses;

pub use pairing::violation as pairing_violation;
pub use reply::{Reply, Step, read_then_answer};
pub use responses::Output;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// `/v1/messages` or `/v1/responses`.
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Recorded {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Picks the reply to a request from its body.
pub type Responder = Box<dyn Fn(&[u8]) -> Reply + Send + Sync>;

#[derive(Default)]
struct Shared {
    script: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
    responder: Option<Responder>,
    /// What `GET /v1/models` answers.
    models: Mutex<serde_json::Value>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("responder", &self.responder.is_some()).finish_non_exhaustive()
    }
}

/// A running mock. Dropping it stops the server.
#[derive(Debug)]
pub struct MockApi {
    addr: SocketAddr,
    shared: Arc<Shared>,
    server: tokio::task::JoinHandle<()>,
}

impl MockApi {
    /// Starts on a free loopback port with `replies` queued.
    pub async fn start(replies: impl IntoIterator<Item = Reply>) -> anyhow::Result<Self> {
        Self::bind(SocketAddr::from(([127, 0, 0, 1], 0)), replies).await
    }

    pub async fn bind(addr: SocketAddr, replies: impl IntoIterator<Item = Reply>) -> anyhow::Result<Self> {
        let shared = Shared { script: Mutex::new(replies.into_iter().collect()), ..Shared::default() };
        Self::serve(addr, shared).await
    }

    /// Answers every request with `responder(body)`, for as long as it runs.
    pub async fn responder(
        addr: SocketAddr,
        responder: impl Fn(&[u8]) -> Reply + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        Self::serve(addr, Shared { responder: Some(Box::new(responder)), ..Shared::default() }).await
    }

    async fn serve(addr: SocketAddr, shared: Shared) -> anyhow::Result<Self> {
        let shared = Arc::new(shared);
        let app = axum::Router::new()
            .route("/v1/messages", post(messages))
            .route("/v1/responses", post(messages))
            .route("/v1/chat/completions", post(messages))
            .route("/v1/models", get(models))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { addr, shared, server })
    }

    /// Value for `ANTHROPIC_BASE_URL`-style settings.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn push(&self, reply: Reply) {
        lock(&self.shared.script).push_back(reply);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.shared.requests).clone()
    }

    /// Forgets recorded requests (long soak runs would otherwise keep every body).
    pub fn clear_requests(&self) {
        lock(&self.shared.requests).clear();
    }

    pub fn remaining(&self) -> usize {
        lock(&self.shared.script).len()
    }

    /// Sets what `GET /v1/models` answers (default: `{"models":[]}`).
    pub fn set_models(&self, catalog: serde_json::Value) {
        *lock(&self.shared.models) = catalog;
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn models(State(shared): State<Arc<Shared>>) -> Response {
    let catalog = lock(&shared.models).clone();
    let catalog = if catalog.is_null() { serde_json::json!({"models": []}) } else { catalog };
    ([("content-type", "application/json")], catalog.to_string()).into_response()
}

async fn messages(State(shared): State<Arc<Shared>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    // Refused as the API refuses it, before a scripted reply is taken.
    let refused = (uri.path() == "/v1/messages").then(|| pairing::violation(&body)).flatten();
    if let Some(message) = refused {
        if shared.responder.is_none() {
            lock(&shared.requests).push(Recorded { path: uri.path().to_owned(), headers, body });
        }
        let mut res = (StatusCode::BAD_REQUEST, pairing::refusal(&message)).into_response();
        res.headers_mut().insert("content-type", axum::http::HeaderValue::from_static("application/json"));
        res.headers_mut().insert("x-should-retry", axum::http::HeaderValue::from_static("false"));
        return res;
    }
    let reply = match &shared.responder {
        Some(respond) => respond(&body),
        None => {
            lock(&shared.requests).push(Recorded { path: uri.path().to_owned(), headers, body });
            let Some(reply) = lock(&shared.script).pop_front() else {
                return (StatusCode::INTERNAL_SERVER_ERROR, "mock script exhausted").into_response();
            };
            reply
        }
    };
    match reply {
        Reply::Status { status, headers, body } => {
            let mut res =
                (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body).into_response();
            for (k, v) in headers {
                if let (Ok(k), Ok(v)) = (k.parse::<axum::http::HeaderName>(), v.parse()) {
                    res.headers_mut().insert(k, v);
                }
            }
            res
        }
        Reply::Stream(steps) => Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(reply::stream(steps)))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}
