//! Opening a connection over a server's transport.

use std::collections::HashMap;

use rmcp::service::serve_client;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

use super::{Connection, Handler, Server};
use crate::config::McpTransport;
use crate::oauth::AuthedClient;
use crate::process;
use crate::sse::SseTransport;

impl Server {
    pub(super) async fn open(&self) -> Result<Connection, String> {
        let handler = Handler { server: self.me.clone() };
        let failed = |e: &dyn std::fmt::Display| format!("Failed to connect: {e}");
        match &self.config.transport {
            McpTransport::Stdio { command, args, env, cwd } => {
                let (child, stdout, stdin) = process::spawn(command, args, env, cwd.as_deref())
                    .map_err(|e| format!("Failed to start \"{command}\": {e}"))?;
                let service = serve_client(handler, (stdout, stdin)).await.map_err(|e| failed(&e))?;
                Ok(Connection { service, child: Some(child) })
            }
            McpTransport::Http { url, headers, .. } => {
                let mut custom = HashMap::with_capacity(headers.len());
                for (name, value) in headers {
                    let name = http::HeaderName::try_from(name.as_str()).map_err(|e| failed(&e))?;
                    let value = http::HeaderValue::try_from(value.as_str()).map_err(|e| failed(&e))?;
                    custom.insert(name, value);
                }
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str()).custom_headers(custom);
                let client = AuthedClient::new(self.http.clone(), self.auth.clone());
                let transport = StreamableHttpClientTransport::with_client(client, config);
                let service = serve_client(handler, transport).await.map_err(|e| failed(&e))?;
                Ok(Connection { service, child: None })
            }
            McpTransport::Sse { url, headers, .. } => {
                let mut map = http::HeaderMap::with_capacity(headers.len());
                for (name, value) in headers {
                    let name = http::HeaderName::try_from(name.as_str()).map_err(|e| failed(&e))?;
                    let value = http::HeaderValue::try_from(value.as_str()).map_err(|e| failed(&e))?;
                    map.insert(name, value);
                }
                let transport = SseTransport::connect(self.http.clone(), self.auth.clone(), url, map)
                    .await
                    .map_err(|e| failed(&e))?;
                let service = serve_client(handler, transport).await.map_err(|e| failed(&e))?;
                Ok(Connection { service, child: None })
            }
        }
    }
}
