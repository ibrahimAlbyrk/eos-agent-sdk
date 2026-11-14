//! A local stand-in for the frame API, a fake token source and a throwaway
//! agent workspace.

#![allow(dead_code, unreachable_pub)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use eos_agent_artifacts::{FrameConfig, tools};
use eos_agent_core::{
    BoxFuture, CancellationToken, Credential, Extensions, ModelError, TokenSource, Tool, ToolContext, ToolOutput,
    Workspace,
};
use eos_agent_types::ToolResultContent;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Path and query, as sent.
    pub uri: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, v: Value) -> Self {
        Self { status, headers: vec![("content-type", "application/json".into())], body: v.to_string().into_bytes() }
    }

    pub fn content(body: &str, kind: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", kind.into()), ("x-frame-content-route", "served".into())],
            body: body.as_bytes().to_vec(),
        }
    }
}

type Handler = Box<dyn Fn(&Req) -> Reply + Send + Sync>;

struct Route {
    method: &'static str,
    /// Exact path+query, or a prefix when it ends with `*`.
    pattern: String,
    once: bool,
    handler: Handler,
}

#[derive(Default)]
struct Shared {
    routes: Mutex<Vec<Route>>,
    log: Mutex<Vec<Req>>,
}

pub struct Mock {
    pub base: String,
    shared: Arc<Shared>,
}

impl Mock {
    pub async fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let app =
            axum::Router::new().fallback(handle).layer(DefaultBodyLimit::disable()).with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { base, shared }
    }

    pub fn on(&self, method: &'static str, pattern: &str, handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) {
        self.route(method, pattern, false, handler);
    }

    /// Answers the first matching request only.
    pub fn once(&self, method: &'static str, pattern: &str, handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) {
        self.route(method, pattern, true, handler);
    }

    fn route(
        &self,
        method: &'static str,
        pattern: &str,
        once: bool,
        handler: impl Fn(&Req) -> Reply + Send + Sync + 'static,
    ) {
        let route = Route { method, pattern: pattern.to_owned(), once, handler: Box::new(handler) };
        self.shared.routes.lock().expect("routes").push(route);
    }

    pub fn requests(&self) -> Vec<Req> {
        self.shared.log.lock().expect("log").clone()
    }

    pub fn find(&self, method: &str, prefix: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.method == method && r.uri.starts_with(prefix)).collect()
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let uri = parts.uri.path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let req = Req { method: parts.method.to_string(), uri, headers: parts.headers, body };
    shared.log.lock().expect("log").push(req.clone());
    let reply = {
        let mut routes = shared.routes.lock().expect("routes");
        let hit = routes.iter().position(|r| {
            r.method == req.method
                && match r.pattern.strip_suffix('*') {
                    Some(prefix) => req.uri.starts_with(prefix),
                    None => req.uri == r.pattern,
                }
        });
        match hit {
            Some(i) if routes[i].once => (routes.remove(i).handler)(&req),
            Some(i) => (routes[i].handler)(&req),
            None => Reply::json(404, serde_json::json!({ "error": "not_found" })),
        }
    };
    let mut res = Response::builder().status(StatusCode::from_u16(reply.status).expect("status"));
    for (k, v) in reply.headers {
        res = res.header(k, v);
    }
    res.body(Body::from(reply.body)).expect("response")
}

/// Hands out `first`; when the server refuses it, hands out `renewed` once.
pub struct Tokens {
    current: Mutex<String>,
    renewed: Mutex<Option<String>>,
    pub refusals: AtomicU32,
}

impl Tokens {
    pub fn new(first: &str, renewed: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(first.into()),
            renewed: Mutex::new(renewed.map(str::to_owned)),
            refusals: AtomicU32::new(0),
        })
    }
}

impl TokenSource for Tokens {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        let token = self.current.lock().expect("token").clone();
        Box::pin(async move { Ok(Credential::Bearer(token)) })
    }

    fn refused<'a>(&'a self, _rejected: &'a Credential) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            match self.renewed.lock().expect("renewed").take() {
                Some(next) => {
                    *self.current.lock().expect("token") = next;
                    true
                }
                None => false,
            }
        })
    }
}

/// One agent: a fresh working directory and scratch directory.
pub struct Agent {
    pub dir: PathBuf,
    pub cwd: PathBuf,
    pub scratch: PathBuf,
    ext: Extensions,
    cancel: CancellationToken,
}

impl Agent {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "eos-artifacts-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let cwd = dir.join("work");
        let scratch = dir.join("scratch");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let ext = Extensions::default();
        ext.insert(Workspace::new(cwd.clone(), Vec::new(), scratch.clone()));
        Self { dir, cwd, scratch, ext, cancel: CancellationToken::new() }
    }

    /// The agent's own state, as a tool call sees it.
    #[allow(dead_code)]
    pub fn ext(&self) -> &Extensions {
        &self.ext
    }

    pub fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.cwd.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, bytes).expect("write");
        path
    }

    /// The same agent after a restart: its state rebuilt from `history` only.
    pub async fn resume(&mut self, tool: &Arc<dyn Tool>, history: &[eos_agent_core::StoredMessage]) {
        self.ext = Extensions::default();
        self.ext.insert(Workspace::new(self.cwd.clone(), Vec::new(), self.scratch.clone()));
        tool.restore(history, &self.ext).await;
    }

    pub async fn call(&self, tool: &Arc<dyn Tool>, input: Value) -> (bool, String) {
        let raw = serde_json::value::to_raw_value(&input).expect("raw");
        let ctx = ToolContext {
            agent_id: "a1",
            tool_use_id: "t1",
            cancel: &self.cancel,
            extensions: &self.ext,
            transcript: &[],
        };
        let ToolOutput { content, is_error, .. } = tool.call(&raw, ctx).await;
        match content {
            ToolResultContent::Text(t) => (is_error, t),
            ToolResultContent::Blocks(_) => panic!("text expected"),
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The three tools against `mock`: `[Artifact, ArtifactComments, ArtifactData]`.
pub fn tools_for(mock: &Mock, tokens: Arc<Tokens>) -> Vec<Arc<dyn Tool>> {
    let config = FrameConfig {
        base_url: mock.base.clone(),
        content_host: format!("{}/_host/{{slug}}", mock.base),
        session_id: Some("session_test".into()),
        ..FrameConfig::default()
    };
    tools(tokens, config).expect("tools")
}

/// The frame config every test uses against `mock`.
#[allow(dead_code)]
pub fn frame_config(mock: &Mock) -> FrameConfig {
    FrameConfig {
        base_url: mock.base.clone(),
        content_host: format!("{}/_host/{{slug}}", mock.base),
        session_id: Some("session_test".into()),
        ..FrameConfig::default()
    }
}

/// Where the content host serves a published file's stored source (`path`
/// "" = the page), with the boot's asset token `at`.
pub fn source(slug: &str, ver: &str, path: &str) -> String {
    let path = if path.is_empty() { "index.html" } else { path };
    format!("/_host/{slug}/_f/{ver}/_src/{path}?__frame_t=at")
}
