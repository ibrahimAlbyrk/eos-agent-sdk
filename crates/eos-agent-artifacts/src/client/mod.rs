//! A client for claude.ai's frame API (`/api/frame/*`), the service behind
//! Claude Code's Artifact tools. It authenticates with the Claude sign-in's
//! OAuth bearer and identifies itself as what it is: no Claude Code user agent
//! or client version is sent unless the host chooses to.

mod body;
mod endpoints;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use eos_agent_core::{Credential, TokenSource};
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use tokio::io::AsyncWriteExt;

pub(crate) use body::{Entry, PublishBody, SourceFile, UploadBatch};
pub(crate) use endpoints::{Boot, Deployed};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Where an artifact's published files are served, `{slug}` replaced.
pub const DEFAULT_CONTENT_HOST: &str = "https://{slug}.frame.claudeusercontent.com";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const ACCEPT: &str = "application/json, application/vnd.ant.frame-refusal+json";
/// JSON answers larger than this are refused rather than buffered.
const MAX_JSON_BYTES: usize = 4 << 20;

#[derive(Debug, Clone)]
pub struct FrameConfig {
    pub base_url: String,
    /// The content host (`{slug}` is replaced); it is sent no credential,
    /// only the artifact's own asset token.
    pub content_host: String,
    /// Sent as `User-Agent`; name the host honestly.
    pub user_agent: String,
    /// `X-Frame-Session-Id`: the host's session id, when it has one.
    pub session_id: Option<String>,
    /// `X-Frame-Platform`.
    pub platform: String,
    /// `X-Frame-Client-Version`; left out by default.
    pub client_version: Option<String>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
}

impl Default for FrameConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            content_host: DEFAULT_CONTENT_HOST.into(),
            user_agent: concat!("eos-agent/", env!("CARGO_PKG_VERSION")).into(),
            session_id: None,
            platform: "cli".into(),
            client_version: None,
            timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("not signed in to Claude: {0}")]
    Auth(String),
    #[error("the artifacts API needs a Claude sign-in (an API key can't reach it)")]
    NeedsSignIn,
    #[error("couldn't reach the artifacts API: {0}")]
    Network(String),
    #[error("the artifacts API answered HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the artifacts API sent an answer this client can't read: {0}")]
    Malformed(String),
    #[error("{0}")]
    Local(String),
}

impl FrameError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// How a request body is (re)built: a 401 retry sends it a second time, so
/// streamed bodies are recreated from their files rather than kept.
#[derive(Clone)]
pub(crate) enum RequestBody {
    Empty,
    Json(Bytes),
    Stream { content_type: String, make: Arc<dyn Fn() -> reqwest::Body + Send + Sync> },
}

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Json(b) => write!(f, "Json({} bytes)", b.len()),
            Self::Stream { content_type, .. } => write!(f, "Stream({content_type})"),
        }
    }
}

impl RequestBody {
    pub(crate) fn json(value: &serde_json::Value) -> Self {
        Self::Json(Bytes::from(value.to_string()))
    }
}

pub struct FrameClient {
    http: reqwest::Client,
    config: FrameConfig,
    auth: Arc<dyn TokenSource>,
}

impl std::fmt::Debug for FrameClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameClient").field("base_url", &self.config.base_url).finish_non_exhaustive()
    }
}

impl FrameClient {
    pub fn new(auth: Arc<dyn TokenSource>, config: FrameConfig) -> Result<Self, FrameError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| FrameError::Local(format!("HTTP client: {e}")))?;
        Ok(Self { http, config, auth })
    }

    async fn bearer(&self) -> Result<(Credential, String), FrameError> {
        match self.auth.credential().await {
            Ok(Credential::Bearer(token)) => {
                let cred = Credential::Bearer(token.clone());
                Ok((cred, token))
            }
            Ok(Credential::ApiKey(_)) => Err(FrameError::NeedsSignIn),
            Err(e) => Err(FrameError::Auth(e.to_string())),
        }
    }

    fn request(&self, method: &Method, path: &str, token: &str, extra: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.config.base_url))
            .timeout(self.config.timeout)
            .bearer_auth(token)
            .header("anthropic-beta", OAUTH_BETA)
            .header("accept", ACCEPT)
            .header("user-agent", &self.config.user_agent)
            .header("x-frame-cp", "go")
            .header("x-frame-surface", "code")
            .header("x-frame-platform", &self.config.platform);
        if let Some(v) = &self.config.client_version {
            req = req.header("x-frame-client-version", v);
        }
        if let Some(id) = &self.config.session_id {
            req = req.header("x-frame-session-id", id);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        req
    }

    /// Sends one request; a 401 asks the token source for a fresh credential
    /// and retries once.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        body: &RequestBody,
        extra: &[(&str, &str)],
    ) -> Result<reqwest::Response, FrameError> {
        let (mut cred, mut token) = self.bearer().await?;
        let mut renewed = false;
        loop {
            let mut req = self.request(&method, path, &token, extra);
            req = match body {
                RequestBody::Empty => req,
                RequestBody::Json(b) => req.header("content-type", "application/json").body(b.clone()),
                RequestBody::Stream { content_type, make } => req.header("content-type", content_type).body(make()),
            };
            let res = req.send().await.map_err(|e| FrameError::Network(e.to_string()))?;
            if res.status() == StatusCode::UNAUTHORIZED && !renewed && self.auth.refused(&cred).await {
                renewed = true;
                (cred, token) = self.bearer().await?;
                continue;
            }
            return Ok(res);
        }
    }

    /// A JSON request whose answer is read whole (bounded); non-2xx answers
    /// become [`FrameError::Status`].
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        body: &RequestBody,
    ) -> Result<serde_json::Value, FrameError> {
        let (status, _, bytes) = self.call_raw(method, path, body, &[]).await?;
        if !(200..300).contains(&status) {
            return Err(FrameError::Status { status, body: error_text(&bytes) });
        }
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| FrameError::Malformed(e.to_string()))
    }

    /// Like [`call`](Self::call) but hands back any status with its body.
    pub(crate) async fn call_raw(
        &self,
        method: Method,
        path: &str,
        body: &RequestBody,
        extra: &[(&str, &str)],
    ) -> Result<(u16, HeaderMap, Bytes), FrameError> {
        let res = self.send(method, path, body, extra).await?;
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let bytes = read_capped(res, MAX_JSON_BYTES).await?;
        Ok((status, headers, bytes))
    }

    /// Streams a GET answer to `dest` without holding it in memory. Returns
    /// the status, headers and bytes written (0 when the status wasn't 2xx).
    pub(crate) async fn download(
        &self,
        path: &str,
        extra: &[(&str, &str)],
        dest: &Path,
        max_bytes: u64,
    ) -> Result<(u16, HeaderMap, u64), FrameError> {
        let res = self.send(Method::GET, path, &RequestBody::Empty, extra).await?;
        save(res, dest, max_bytes).await
    }

    /// Streams `url` on the content host to `dest`. Only the URL's own asset
    /// token goes along: never the sign-in's bearer, nor its headers.
    pub(crate) async fn download_public(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<(u16, HeaderMap, u64), FrameError> {
        let res = self
            .http
            .get(url)
            .timeout(self.config.timeout)
            .header("user-agent", &self.config.user_agent)
            .send()
            .await
            .map_err(|e| FrameError::Network(e.to_string()))?;
        save(res, dest, max_bytes).await
    }

    /// The session the requests name (`x-frame-session-id`).
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.config.session_id.as_deref()
    }

    pub(crate) fn user_agent(&self) -> &str {
        &self.config.user_agent
    }

    /// The content host's origin for one artifact.
    pub(crate) fn content_origin(&self, slug: &str) -> String {
        self.config.content_host.replace("{slug}", slug)
    }
}

/// Streams a 2xx answer to `dest` (see [`FrameClient::download`]).
async fn save(res: reqwest::Response, dest: &Path, max_bytes: u64) -> Result<(u16, HeaderMap, u64), FrameError> {
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    if !res.status().is_success() {
        let bytes = read_capped(res, 64 << 10).await.unwrap_or_default();
        return Err(FrameError::Status { status, body: error_text(&bytes) });
    }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| FrameError::Local(format!("{}: {e}", dir.display())))?;
    }
    let mut file =
        tokio::fs::File::create(dest).await.map_err(|e| FrameError::Local(format!("{}: {e}", dest.display())))?;
    let mut written = 0u64;
    let mut chunks = res.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| FrameError::Network(e.to_string()))?;
        written += chunk.len() as u64;
        if written > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(FrameError::Local(format!("the file is larger than {max_bytes} bytes")));
        }
        file.write_all(&chunk).await.map_err(|e| FrameError::Local(e.to_string()))?;
    }
    file.flush().await.map_err(|e| FrameError::Local(e.to_string()))?;
    Ok((status, headers, written))
}

async fn read_capped(res: reqwest::Response, max: usize) -> Result<Bytes, FrameError> {
    let mut out = Vec::new();
    let mut chunks = res.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| FrameError::Network(e.to_string()))?;
        if out.len() + chunk.len() > max {
            return Err(FrameError::Malformed(format!("answer larger than {max} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

/// The server's error text: a JSON `error` (+ `reason`) or the plain body, cut short.
pub(crate) fn error_text(body: &[u8]) -> String {
    let text = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) => match (v.get("error"), v.get("reason").and_then(|r| r.as_str())) {
            (Some(serde_json::Value::String(e)), Some(r)) => format!("{e} [{r}]"),
            (Some(serde_json::Value::String(e)), None) => e.clone(),
            (Some(serde_json::Value::Object(o)), _) => {
                o.get("message").or_else(|| o.get("code")).and_then(|m| m.as_str()).unwrap_or_default().to_owned()
            }
            _ => v.to_string(),
        },
        Err(_) => String::from_utf8_lossy(body).trim().to_owned(),
    };
    cut(&text, 300)
}

pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Percent-encodes one path segment.
pub(crate) fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
