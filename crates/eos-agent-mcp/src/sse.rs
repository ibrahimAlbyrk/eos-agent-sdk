//! The legacy HTTP+SSE transport (MCP 2024-11-05), which rmcp has no client
//! for: a GET opens the event stream, whose `endpoint` event names where to
//! POST messages; replies arrive as `message` events. Requests carry the
//! server's headers and its OAuth token, renewed once after a 401, as on the
//! Streamable HTTP transport.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderMap, StatusCode};
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use sse_stream::SseStream;
use tokio::sync::mpsc;

use crate::oauth::ServerAuth;

/// How long the server may take to name its message endpoint.
const ENDPOINT_WAIT: Duration = Duration::from_secs(30);

pub(crate) struct SseTransport {
    inbox: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    poster: Arc<Poster>,
    reader: tokio::task::JoinHandle<()>,
}

struct Poster {
    http: reqwest::Client,
    auth: Option<Arc<ServerAuth>>,
    headers: HeaderMap,
    endpoint: String,
}

fn io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Sends `request` with the current token; after a 401 that a renewal fixes,
/// once more. A 401 or 403 left is recorded on the server's auth state.
async fn authorized(
    auth: Option<&Arc<ServerAuth>>,
    request: impl Fn(Option<&str>) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    let token = match auth {
        Some(auth) => auth.bearer().await,
        None => None,
    };
    let res = request(token.as_deref()).send().await.map_err(io)?;
    let Some(auth) = auth else { return Ok(res) };
    let challenge =
        |res: &reqwest::Response| res.headers().get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    match res.status() {
        StatusCode::UNAUTHORIZED => {
            let header = challenge(&res);
            if auth.unauthorized(token.as_deref(), header.as_deref()).await {
                return request(auth.bearer().await.as_deref()).send().await.map_err(io);
            }
            Ok(res)
        }
        StatusCode::FORBIDDEN => {
            if let Some(header) = challenge(&res) {
                auth.forbidden(token.as_deref(), &header).await;
            }
            Ok(res)
        }
        _ => Ok(res),
    }
}

fn with_auth(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
        None => builder,
    }
}

impl SseTransport {
    pub(crate) async fn connect(
        http: reqwest::Client,
        auth: Option<Arc<ServerAuth>>,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Self, std::io::Error> {
        let res = authorized(auth.as_ref(), |token| {
            with_auth(http.get(url).headers(headers.clone()).header(ACCEPT, "text/event-stream"), token)
        })
        .await?;
        if !res.status().is_success() {
            return Err(io(format!("SSE error: Non-200 status code ({})", res.status().as_u16())));
        }
        let mut events = SseStream::from_bytes_stream(res.bytes_stream());
        let first = tokio::time::timeout(ENDPOINT_WAIT, async {
            while let Some(event) = events.next().await {
                let event = event.map_err(io)?;
                if event.event.as_deref() == Some("endpoint") {
                    return Ok(event.data.unwrap_or_default());
                }
            }
            Err(io("SSE stream ended before the server sent its endpoint"))
        })
        .await
        .map_err(|_| io("the server sent no endpoint event"))??;
        let base = reqwest::Url::parse(url).map_err(io)?;
        let endpoint = base.join(first.trim()).map_err(io)?.to_string();
        let (tx, inbox) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            while let Some(Ok(event)) = events.next().await {
                if !matches!(event.event.as_deref(), None | Some("message")) {
                    continue;
                }
                let Some(data) = event.data else { continue };
                if let Ok(message) = serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(&data)
                    && tx.send(message).await.is_err()
                {
                    return;
                }
            }
        });
        Ok(Self { inbox, poster: Arc::new(Poster { http, auth, headers, endpoint }), reader })
    }
}

impl Poster {
    async fn post(&self, body: String) -> Result<(), std::io::Error> {
        let res = authorized(self.auth.as_ref(), |token| {
            let request = self.http.post(&self.endpoint).headers(self.headers.clone());
            with_auth(request.header(CONTENT_TYPE, "application/json").body(body.clone()), token)
        })
        .await?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(io(format!("Error POSTing to endpoint (HTTP {})", res.status().as_u16())))
        }
    }
}

impl Transport<RoleClient> for SseTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let poster = Arc::clone(&self.poster);
        let body = serde_json::to_string(&item);
        async move { poster.post(body.map_err(io)?).await }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inbox.recv()
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.reader.abort();
        std::future::ready(Ok(()))
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
