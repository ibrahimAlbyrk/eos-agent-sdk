//! JSON-RPC 2.0 over newline-delimited JSON, both ways: the client calls the
//! server, and the server calls the client (permission and question requests).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};

pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const TURN_FAILED: i64 = -32000;
pub(crate) const UNKNOWN_AGENT: i64 = -32001;
pub(crate) const NOT_INITIALIZED: i64 = -32002;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub(crate) fn params(message: impl std::fmt::Display) -> Self {
        Self::new(INVALID_PARAMS, message.to_string())
    }
}

/// One line from the client: a request, a notification, or a response to
/// one of the server's own requests.
#[derive(Debug, Deserialize)]
pub(crate) struct Incoming {
    pub(crate) id: Option<Box<RawValue>>,
    pub(crate) method: Option<String>,
    #[serde(default)]
    pub(crate) params: Option<Value>,
    #[serde(default)]
    pub(crate) result: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<RpcError>,
}

type Waiter = oneshot::Sender<Result<Value, RpcError>>;

/// The connection's outgoing side: lines go to one writer task, so writes
/// never interleave and a slow reader slows the agents down instead of
/// letting output pile up.
#[derive(Debug)]
pub(crate) struct Peer {
    out: mpsc::Sender<String>,
    waiting: Mutex<HashMap<String, Waiter>>,
    next_id: AtomicU64,
    /// The client stopped reading requests; nobody will answer ours.
    closed: AtomicBool,
}

impl Peer {
    pub(crate) fn new(out: mpsc::Sender<String>) -> Self {
        Self { out, waiting: Mutex::default(), next_id: AtomicU64::new(1), closed: AtomicBool::new(false) }
    }

    /// Fails every request waiting for the client, and every later one.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.waiting().clear();
    }

    async fn send(&self, mut line: String) {
        // Raw JSON passed through (a tool's input as the model wrote it) may
        // break lines between tokens. Valid JSON has no raw line breaks inside
        // strings, so turning them into spaces keeps one message per line.
        if line.contains(['\n', '\r']) {
            line = line.replace(['\n', '\r'], " ");
        }
        // A closed writer means the client left; nothing more to say.
        let _ = self.out.send(line).await;
    }

    /// `params` is a JSON object, already serialized.
    pub(crate) async fn notify(&self, method: &str, params: &str) {
        self.send(format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#)).await;
    }

    pub(crate) async fn respond(&self, id: &RawValue, result: Result<Value, RpcError>) {
        #[derive(Serialize)]
        struct Response<'a> {
            jsonrpc: &'static str,
            id: &'a RawValue,
            #[serde(skip_serializing_if = "Option::is_none")]
            result: Option<Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<RpcError>,
        }
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        let response = Response { jsonrpc: "2.0", id, result, error };
        if let Ok(line) = serde_json::to_string(&response) {
            self.send(line).await;
        }
    }

    /// Calls the client and waits for its answer, at most `timeout`.
    pub(crate) async fn request(&self, method: &str, params: &str, timeout: Duration) -> Result<Value, String> {
        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        if self.closed.load(Ordering::Relaxed) {
            return Err("the client went away".into());
        }
        self.waiting().insert(id.clone(), tx);
        self.send(format!(r#"{{"jsonrpc":"2.0","id":"{id}","method":"{method}","params":{params}}}"#)).await;
        let answer = tokio::time::timeout(timeout, rx).await;
        self.waiting().remove(&id);
        match answer {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error.message),
            Ok(Err(_)) => Err("the client went away".into()),
            Err(_) => Err(format!("no answer within {} s", timeout.as_secs())),
        }
    }

    /// Hands a client's response to the request waiting for it.
    pub(crate) fn resolve(&self, id: &RawValue, outcome: Result<Value, RpcError>) {
        let Ok(id) = serde_json::from_str::<String>(id.get()) else { return };
        if let Some(waiter) = self.waiting().remove(&id) {
            let _ = waiter.send(outcome);
        }
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, HashMap<String, Waiter>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Writes lines until every sender is gone, flushing whenever it catches up.
pub(crate) async fn write_lines(mut lines: mpsc::Receiver<String>, out: impl AsyncWrite + Unpin) {
    let mut out = BufWriter::new(out);
    while let Some(line) = lines.recv().await {
        let mut next = Some(line);
        while let Some(line) = next {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err() {
                return;
            }
            next = lines.try_recv().ok();
        }
        if out.flush().await.is_err() {
            return;
        }
    }
}
