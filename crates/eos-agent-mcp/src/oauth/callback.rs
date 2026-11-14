//! The callback side of a sign-in: the port, the listener on 127.0.0.1 with
//! Claude Code's pages, and the issuer check on what comes back.

use eos_agent_oauth::{CallbackListener, CallbackPages, page};
use reqwest::Url;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::plan::scheme_host;
use super::provider::random_u16;
use super::store::Entry;

/// Claude Code's fallback callback port.
const FALLBACK_PORT: u16 = 3118;

pub(super) enum Callback {
    /// The code and the `iss` the authorization server added (RFC 9207).
    Code(String, Option<String>),
    Denied(String),
}

/// The port a stored client was registered with, when its redirect is
/// exactly `http://localhost:<port>/callback` (127.0.0.1 counts as localhost).
pub(super) fn registered_port(entry: &Entry) -> Option<u16> {
    let (_, redirect) = (entry.client_id.as_ref()?, entry.redirect_uri.as_ref()?);
    let host = scheme_host(redirect);
    if host != "http://127.0.0.1" && host != "http://localhost" {
        return None;
    }
    let port = Url::parse(redirect).ok()?.port()?;
    (redirect.replace("//127.0.0.1:", "//localhost:") == format!("http://localhost:{port}/callback")).then_some(port)
}

fn free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Claude Code's callback port: `MCP_OAUTH_CALLBACK_PORT` (unchecked), the
/// port the client was registered with, a random free one in 49152–65535,
/// 3118, then any.
pub(super) fn pick_port(registered: Option<u16>) -> Result<u16, String> {
    if let Some(p) =
        std::env::var("MCP_OAUTH_CALLBACK_PORT").ok().and_then(|v| v.trim().parse::<u16>().ok()).filter(|p| *p > 0)
    {
        return Ok(p);
    }
    if let Some(p) = registered.filter(|p| free(*p)) {
        return Ok(p);
    }
    for _ in 0..100 {
        let p = 49152 + random_u16() % (65535 - 49152 + 1);
        if free(p) {
            return Ok(p);
        }
    }
    if free(FALLBACK_PORT) {
        return Ok(FALLBACK_PORT);
    }
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|_| "No available ports for OAuth redirect".into())
}

/// The MCP SDK's RFC 9207 check, which Claude Code's sign-in goes through:
/// the callback's `iss` must name the authorization server, and must be
/// there when its metadata says it sends one.
pub(super) fn check_issuer(metadata: Option<&serde_json::Map<String, Value>>, iss: Option<&str>) -> Result<(), String> {
    let Some(metadata) = metadata else { return Ok(()) };
    let Some(expected) = metadata.get("issuer").and_then(Value::as_str) else { return Ok(()) };
    let supported = metadata.get("authorization_response_iss_parameter_supported") == Some(&Value::Bool(true));
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
    match iss {
        Some(iss) if iss == expected => Ok(()),
        None if !supported => Ok(()),
        received => Err(format!(
            "Issuer mismatch in authorization response (RFC 9207): expected {}, received {}",
            quoted(expected),
            received.map_or_else(|| "undefined".to_owned(), quoted)
        )),
    }
}

/// Answers the browser on `/callback`: a matching `state` with a code (or an
/// error) ends the wait; anything else gets an explanation.
pub(super) async fn serve_callbacks(
    listener: TcpListener,
    state: String,
    redirect: String,
    tx: mpsc::UnboundedSender<Callback>,
) {
    let outcome = CallbackListener::new(listener).wait("/callback", &state, &redirect, &ClaudeCodePages).await;
    let callback = match outcome {
        None => return,
        Some(eos_agent_oauth::Callback::Code { code, params }) => {
            let iss = params.into_iter().find(|(k, _)| k == "iss").map(|(_, v)| v);
            Callback::Code(code, iss)
        }
        Some(eos_agent_oauth::Callback::Denied { error, description, uri }) => {
            let mut why = format!("OAuth error: {error}");
            if let Some(d) = &description {
                why.push_str(&format!(" - {d}"));
            }
            if let Some(uri) = uri {
                why.push_str(&format!(" (See: {uri})"));
            }
            Callback::Denied(why)
        }
    };
    let _ = tx.send(callback);
}

/// Claude Code's callback pages.
struct ClaudeCodePages;

impl CallbackPages for ClaudeCodePages {
    fn not_found(&self, redirect_uri: &str) -> String {
        let msg = format!(
            "This is the Claude Code MCP OAuth callback listener. It only handles /callback. If your OAuth provider \
             redirected here, the registered redirect_uri must be {redirect_uri}."
        );
        page(false, "Not found", &msg, None)
    }

    fn bad_state(&self) -> String {
        page(
            false,
            "Authentication failed",
            "Invalid state parameter. Close this tab and try again from Claude Code.",
            None,
        )
    }

    fn denied(&self, detail: &str) -> String {
        page(false, "Authentication failed", "Close this tab and try again from Claude Code.", Some(detail))
    }

    fn success(&self) -> String {
        page(true, "Authentication successful", "You can close this tab and return to Claude Code.", None)
    }

    fn no_code(&self) -> String {
        page(false, "Authentication failed", "No authorization code received.", None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_issuer_like_the_mcp_sdk() {
        let meta = |supported: bool| {
            let v = serde_json::json!({"issuer": "https://as.dev", "authorization_response_iss_parameter_supported": supported});
            v.as_object().cloned().expect("object")
        };
        assert!(check_issuer(Some(&meta(false)), None).is_ok());
        assert!(check_issuer(Some(&meta(false)), Some("https://as.dev")).is_ok());
        assert_eq!(
            check_issuer(Some(&meta(false)), Some("https://evil.dev")),
            Err("Issuer mismatch in authorization response (RFC 9207): expected \"https://as.dev\", received \"https://evil.dev\"".into())
        );
        assert!(check_issuer(Some(&meta(true)), None).is_err_and(|e| e.ends_with("received undefined")));
    }

    #[test]
    fn registered_ports_need_the_exact_loopback_callback() {
        let entry =
            |uri: &str| Entry { client_id: Some("c".into()), redirect_uri: Some(uri.into()), ..Entry::default() };
        assert_eq!(registered_port(&entry("http://localhost:5000/callback")), Some(5000));
        assert_eq!(registered_port(&entry("http://127.0.0.1:5000/callback")), Some(5000));
        assert_eq!(registered_port(&entry("http://localhost:5000/cb")), None);
        assert_eq!(registered_port(&entry("https://localhost:5000/callback")), None);
        assert_eq!(super::super::plan::with_offline("read"), "read offline_access");
    }
}
