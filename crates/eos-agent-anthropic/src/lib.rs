//! The Anthropic Messages API as a [`ModelClient`].
//!
//! One client (one HTTP/2 connection pool) serves every agent: thousands of
//! agents share a handful of TLS connections instead of opening their own.

pub mod auth;
mod body;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eos_agent_core::{BoxFuture, EventStream, ModelClient, ModelError, ModelRequest, RateLimitInfo, RequestAuth};
use eos_agent_types::StreamEvent;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;

use crate::auth::{Credential, TokenSource};
use eos_agent_http::{Pieces, SseDecoder};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// What a [`RequestAuth`] for this client is labelled.
pub const PROVIDER: &str = "anthropic";
/// How many times one request moves to another credential after its own
/// stopped working (a host could hand out exhausted ones forever).
const MAX_CREDENTIAL_CHANGES: u32 = 8;

/// Betas sent by default; hosts can replace the list.
pub const DEFAULT_BETAS: &[&str] = &[
    "claude-code-20250219",
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    "effort-2025-11-24",
    "extended-cache-ttl-2025-04-11",
];

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: String,
    pub betas: Vec<String>,
    pub user_agent: String,
    /// A stream that sends nothing for this long is treated as dead.
    pub idle_timeout: Duration,
    pub connect_timeout: Duration,
    /// Speak HTTP/2 to a plain-HTTP endpoint without negotiating it (h2c),
    /// so agents share one connection there as they do over TLS.
    pub http2_prior_knowledge: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            betas: DEFAULT_BETAS.iter().map(|b| (*b).to_owned()).collect(),
            user_agent: concat!("eos-agent/", env!("CARGO_PKG_VERSION")).into(),
            idle_timeout: Duration::from_secs(180),
            connect_timeout: Duration::from_secs(30),
            http2_prior_knowledge: false,
        }
    }
}

pub struct AnthropicClient {
    http: reqwest::Client,
    config: ClientConfig,
    auth: Arc<dyn TokenSource>,
    rate_limit: Mutex<Option<RateLimitInfo>>,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient").field("base_url", &self.config.base_url).finish_non_exhaustive()
    }
}

impl AnthropicClient {
    pub fn new(auth: Arc<dyn TokenSource>, config: ClientConfig) -> Result<Self, ModelError> {
        let mut http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90));
        if config.http2_prior_knowledge {
            http = http.http2_prior_knowledge();
        }
        let http = http.build().map_err(|e| ModelError::Invalid(format!("HTTP client: {e}")))?;
        Ok(Self { http, config, auth, rate_limit: Mutex::new(None) })
    }

    /// The plan-limit state from the most recent response.
    pub fn rate_limit(&self) -> Option<RateLimitInfo> {
        self.rate_limit.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The request body `request` becomes, as pieces (for tests and benchmarks).
    pub fn body(&self, request: &ModelRequest) -> Vec<bytes::Bytes> {
        body::build(request)
    }

    async fn send(
        &self,
        body: &[bytes::Bytes],
        cred: &Credential,
        extra_betas: &[&str],
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut betas = self.config.betas.join(",");
        for beta in extra_betas.iter().filter(|b| !self.config.betas.iter().any(|c| c == *b)) {
            if !betas.is_empty() {
                betas.push(',');
            }
            betas.push_str(beta);
        }
        let mut req = self
            .http
            .post(format!("{}/v1/messages?beta=true", self.config.base_url))
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("user-agent", &self.config.user_agent);
        match cred {
            Credential::ApiKey(key) => req = req.header("x-api-key", key),
            Credential::Bearer(token) => {
                if !betas.is_empty() {
                    betas.push(',');
                }
                betas.push_str(OAUTH_BETA);
                req = req.bearer_auth(token);
            }
        }
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas);
        }
        req.body(reqwest::Body::wrap(Pieces::new(body.to_vec()))).send().await
    }

    async fn open(&self, request: &ModelRequest) -> Result<EventStream, ModelError> {
        let body = body::build(request);
        let auth = RequestAuth::choose(request.auth.as_ref(), PROVIDER, &self.auth);
        let mut cred = auth.credential().await?;
        let mut renewed = false;
        let mut changes = 0;
        loop {
            let res = self.send(&body, &cred, request.betas).await.map_err(|e| ModelError::Transient(e.to_string()))?;
            if res.status().as_u16() == 401 && !renewed && auth.refused(&cred).await {
                renewed = true;
                cred = auth.credential().await?;
                continue;
            }
            let info = rate_limit_info(res.headers());
            if info.status.is_some() {
                auth.observe(&info);
                *self.rate_limit.lock().unwrap_or_else(PoisonError::into_inner) = Some(info.clone());
            }
            let status = res.status().as_u16();
            if status == 200 {
                return Ok(events(res, self.config.idle_timeout));
            }
            let headers = res.headers().clone();
            let text = res.text().await.unwrap_or_default();
            let error = status_error(status, &headers, &text, info);
            // The credential is used up or signed out: the source may have another.
            let unusable = matches!(error, ModelError::LimitReached { .. } | ModelError::Auth(_));
            if !unusable || changes == MAX_CREDENTIAL_CHANGES || !auth.failed(&cred, &error).await {
                return Err(error);
            }
            changes += 1;
            renewed = false;
            cred = auth.credential().await?;
        }
    }
}

impl ModelClient for AnthropicClient {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        Box::pin(self.open(request))
    }
}

/// The response body as parsed events, with an idle watchdog.
fn events(res: reqwest::Response, idle: Duration) -> EventStream {
    struct State {
        bytes: futures_util::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
        decoder: SseDecoder,
        done: bool,
    }
    let state = State { bytes: res.bytes_stream().boxed(), decoder: SseDecoder::default(), done: false };
    futures_util::stream::unfold(state, move |mut s| async move {
        loop {
            if s.done {
                return None;
            }
            if let Some(frame) = s.decoder.next_frame() {
                let item = match StreamEvent::parse(&frame.event, &frame.data) {
                    Ok(StreamEvent::Error(e)) => {
                        s.done = true;
                        Err(if prompt_too_long(&e.message) {
                            ModelError::ContextWindowExceeded(e.message)
                        } else {
                            ModelError::Api { kind: e.kind, message: e.message }
                        })
                    }
                    Ok(event) => Ok(event),
                    Err(e) => {
                        s.done = true;
                        Err(ModelError::Transient(e.to_string()))
                    }
                };
                return Some((item, s));
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
                    if s.decoder.has_partial() {
                        return Some((Err(ModelError::Transient("stream ended mid-event".into())), s));
                    }
                    return None;
                }
            }
        }
    })
    .boxed()
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn rate_limit_info(h: &HeaderMap) -> RateLimitInfo {
    let num = |n: &str| header(h, n).and_then(|v| v.parse::<f64>().ok());
    RateLimitInfo {
        status: header(h, "anthropic-ratelimit-unified-status").map(str::to_owned),
        resets_at: header(h, "anthropic-ratelimit-unified-reset").and_then(|v| v.parse().ok()),
        claim: header(h, "anthropic-ratelimit-unified-representative-claim").map(str::to_owned),
        five_hour_utilization: num("anthropic-ratelimit-unified-5h-utilization"),
        seven_day_utilization: num("anthropic-ratelimit-unified-7d-utilization"),
    }
}

/// The API's words for a prompt over the model's context window (an
/// `invalid_request_error`: "prompt is too long: 210000 tokens > 200000 maximum").
fn prompt_too_long(message: &str) -> bool {
    message.to_ascii_lowercase().contains("prompt is too long")
}

fn status_error(status: u16, headers: &HeaderMap, body: &str, info: RateLimitInfo) -> ModelError {
    let retry_after = header(headers, "retry-after").and_then(|v| v.parse::<f64>().ok()).map(Duration::from_secs_f64);
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(500).collect());
    if matches!(status, 400 | 413) && prompt_too_long(&message) {
        return ModelError::ContextWindowExceeded(message);
    }
    match header(headers, "x-should-retry") {
        Some("false") => return ModelError::Invalid(format!("{status}: {message}")),
        Some("true") if status != 429 && status != 529 => return ModelError::Transient(format!("{status}: {message}")),
        _ => {}
    }
    match status {
        429 if info.status.as_deref() == Some("rejected") => ModelError::LimitReached { info },
        429 => ModelError::RateLimited { retry_after },
        529 => ModelError::Overloaded { retry_after },
        401 | 403 => ModelError::Auth(format!("{status}: {message}")),
        408 | 409 | 500.. => ModelError::Transient(format!("{status}: {message}")),
        _ => ModelError::Invalid(format!("{status}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().expect("header"));
        }
        h
    }

    #[test]
    fn maps_statuses() {
        let h = headers(&[("retry-after", "3")]);
        assert!(matches!(
            status_error(429, &h, "", RateLimitInfo::default()),
            ModelError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(3)
        ));
        let rejected = RateLimitInfo { status: Some("rejected".into()), ..Default::default() };
        assert!(matches!(status_error(429, &h, "", rejected), ModelError::LimitReached { .. }));
        assert!(matches!(status_error(529, &h, "", RateLimitInfo::default()), ModelError::Overloaded { .. }));
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad thing"}}"#;
        assert!(
            matches!(status_error(400, &h, body, RateLimitInfo::default()), ModelError::Invalid(m) if m.contains("bad thing"))
        );
        let retry = headers(&[("x-should-retry", "true")]);
        assert!(matches!(status_error(400, &retry, body, RateLimitInfo::default()), ModelError::Transient(_)));
    }

    #[test]
    fn a_prompt_over_the_window_is_a_context_overflow() {
        let body = |message: &str| {
            format!(r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{message}"}}}}"#)
        };
        let too_long = body("prompt is too long: 210000 tokens > 200000 maximum");
        let stop = headers(&[("x-should-retry", "false")]);
        for (status, h) in [(400, &stop), (400, &HeaderMap::new()), (413, &stop)] {
            let error = status_error(status, h, &too_long, RateLimitInfo::default());
            assert!(
                matches!(&error, ModelError::ContextWindowExceeded(m) if m == "prompt is too long: 210000 tokens > 200000 maximum"),
                "{status}: {error:?}"
            );
        }
        let upper = status_error(400, &stop, &body("Prompt is too long"), RateLimitInfo::default());
        assert!(matches!(upper, ModelError::ContextWindowExceeded(_)));
        // Other refusals of a request stay what they were.
        let output = body("max_tokens: 99999 > 64000, which is the maximum allowed number of output tokens");
        assert!(matches!(status_error(400, &stop, &output, RateLimitInfo::default()), ModelError::Invalid(_)));
        assert!(matches!(
            status_error(500, &HeaderMap::new(), &too_long, RateLimitInfo::default()),
            ModelError::Transient(_)
        ));
    }

    #[test]
    fn reads_unified_rate_limit_headers() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-reset", "1794416400"),
            ("anthropic-ratelimit-unified-representative-claim", "seven_day"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.82"),
        ]);
        let info = rate_limit_info(&h);
        assert_eq!(info.status.as_deref(), Some("allowed_warning"));
        assert_eq!(info.resets_at, Some(1_794_416_400));
        assert_eq!(info.seven_day_utilization, Some(0.82));
    }
}
