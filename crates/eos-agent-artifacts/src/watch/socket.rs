//! The artifact's live channel: a WebSocket to
//! `wss://claude.ai/edge-api/frame-live/<slug>/ws`, opened with the
//! subprotocols `frame-live.v1` and the boot's `subscriptionToken` (as Claude
//! Code opens it). The upgrade goes through reqwest (HTTP/1.1, the same TLS
//! trust as every other request), the frames through tungstenite.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

const PROTOCOL: &str = "frame-live.v1";
/// Frames on this channel are small JSON notes.
const MAX_MESSAGE: usize = 256 << 10;
const READ_BUFFER: usize = 4 << 10;

pub(crate) type Socket = WebSocketStream<reqwest::Upgraded>;

#[derive(Debug)]
pub(crate) enum OpenError {
    /// The server answered the upgrade with this status (`cf_mitigated`:
    /// with a `cf-mitigated` header, Cloudflare's challenge).
    Refused {
        status: u16,
        cf_mitigated: bool,
    },
    Network(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { status, .. } => write!(f, "the live channel refused the connection (HTTP {status})"),
            Self::Network(e) => write!(f, "the live channel could not be reached: {e}"),
        }
    }
}

/// `origin` is the live channel's `https://` origin (claude.ai).
pub(crate) async fn open(
    http: &reqwest::Client,
    origin: &str,
    slug: &str,
    token: &str,
    user_agent: &str,
    timeout: Duration,
) -> Result<Socket, OpenError> {
    let url = format!("{}/edge-api/frame-live/{slug}/ws", origin.trim_end_matches('/'));
    let key = generate_key();
    let res = http
        .get(&url)
        .timeout(timeout)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", &key)
        .header("sec-websocket-protocol", format!("{PROTOCOL}, {token}"))
        .header("user-agent", user_agent)
        .send()
        .await
        .map_err(|e| OpenError::Network(e.to_string()))?;
    if res.status().as_u16() != 101 {
        let cf_mitigated = res.headers().contains_key("cf-mitigated");
        return Err(OpenError::Refused { status: res.status().as_u16(), cf_mitigated });
    }
    let accept = res.headers().get("sec-websocket-accept").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if accept != derive_accept_key(key.as_bytes()) {
        return Err(OpenError::Network("the live channel answered the upgrade wrongly".into()));
    }
    let raw = res.upgrade().await.map_err(|e| OpenError::Network(e.to_string()))?;
    let mut config = WebSocketConfig::default();
    config.read_buffer_size = READ_BUFFER;
    config.max_message_size = Some(MAX_MESSAGE);
    config.max_frame_size = Some(MAX_MESSAGE);
    Ok(WebSocketStream::from_raw_socket(raw, Role::Client, Some(config)).await)
}

/// What the channel said.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A version went live (the server's `ver`).
    Version(String),
    /// Something changed in the comments.
    Comment,
    Other,
}

/// One text frame: `{"kind": …, "ver"?: …}` (Claude Code reads only these two).
pub(crate) fn parse(frame: &str) -> Event {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(frame) else { return Event::Other };
    match value["kind"].as_str() {
        Some("comment") => Event::Comment,
        Some("summon") => Event::Other,
        _ => value["ver"].as_str().map_or(Event::Other, |v| Event::Version(v.to_owned())),
    }
}

/// Reads the next event; pings when the line has been quiet for `idle`, and
/// gives up (None) when a ping goes unanswered or the socket ends (`closed`
/// gets the server's close code, if it sent one).
pub(crate) async fn next(socket: &mut Socket, idle: Duration, closed: &mut Option<u16>) -> Option<Event> {
    let mut pinged = false;
    loop {
        match tokio::time::timeout(idle, socket.next()).await {
            Err(_) if pinged => return None,
            Err(_) => {
                pinged = true;
                socket.send(Message::Ping(Vec::new().into())).await.ok()?;
            }
            Ok(None | Some(Err(_))) => return None,
            Ok(Some(Ok(message))) => {
                pinged = false;
                match message {
                    Message::Text(text) => return Some(parse(text.as_str())),
                    Message::Close(frame) => {
                        *closed = frame.map(|f| u16::from(f.code));
                        return None;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        assert_eq!(parse(r#"{"kind":"comment","slug":"s"}"#), Event::Comment);
        assert_eq!(parse(r#"{"kind":"publish","ver":"1794124234-1f2a"}"#), Event::Version("1794124234-1f2a".into()));
        assert_eq!(parse(r#"{"ver":"1-a"}"#), Event::Version("1-a".into()));
        assert_eq!(parse(r#"{"kind":"summon","ver":"1-a"}"#), Event::Other);
        assert_eq!(parse("not json"), Event::Other);
    }
}
