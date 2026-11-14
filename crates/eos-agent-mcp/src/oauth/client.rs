//! The Streamable HTTP client with OAuth: every request carries the current
//! access token; a 401 renews it once and retries, as the MCP SDK's transport
//! does with an auth provider; what cannot be fixed here is left on the
//! [`ServerAuth`] for the server to act on.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;

use super::ServerAuth;

type Error = StreamableHttpError<reqwest::Error>;

#[derive(Clone)]
pub(crate) struct AuthedClient {
    http: reqwest::Client,
    auth: Option<Arc<ServerAuth>>,
}

impl AuthedClient {
    pub(crate) fn new(http: reqwest::Client, auth: Option<Arc<ServerAuth>>) -> Self {
        Self { http, auth }
    }

    /// Runs `request` with the current token; after a 401 that a renewal
    /// fixes, once more.
    async fn with_token<T, F, Fut>(&self, request: F) -> Result<T, Error>
    where
        F: Fn(Option<String>) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        let Some(auth) = &self.auth else { return request(None).await };
        let token = auth.bearer().await;
        let first = request(token.clone()).await;
        match &first {
            Err(StreamableHttpError::AuthRequired(e)) => {
                let header = Some(e.www_authenticate_header.as_str()).filter(|h| !h.is_empty());
                if auth.unauthorized(token.as_deref(), header).await {
                    return request(auth.bearer().await).await;
                }
            }
            // A 401 without a challenge header.
            Err(StreamableHttpError::Client(e)) if e.status() == Some(reqwest::StatusCode::UNAUTHORIZED) => {
                if auth.unauthorized(token.as_deref(), None).await {
                    return request(auth.bearer().await).await;
                }
                return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(String::new())));
            }
            Err(StreamableHttpError::InsufficientScope(e)) => {
                auth.forbidden(token.as_deref(), &e.www_authenticate_header).await;
            }
            _ => {}
        }
        first
    }
}

impl StreamableHttpClient for AuthedClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.with_token(|token| {
            self.http.post_message(uri.clone(), message.clone(), session_id.clone(), token, custom_headers.clone())
        })
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.with_token(|token| {
            self.http.post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let token = match &self.auth {
            Some(auth) => auth.bearer().await,
            None => None,
        };
        self.http.delete_session(uri, session_id, token, custom_headers).await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Error> {
        self.with_token(|token| {
            self.http.get_stream(uri.clone(), session_id.clone(), last_event_id.clone(), token, custom_headers.clone())
        })
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Error> {
        self.with_token(|token| {
            self.http.get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }
}
