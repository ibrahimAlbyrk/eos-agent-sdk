//! The loopback listener a browser sign-in comes back to: it answers the
//! browser and hands the authorization code (or the refusal) to the flow.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the browser brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// The callback's query parameters (`code`, and any others such as an
    /// issued `client_id`), in order.
    Code {
        code: String,
        params: Vec<(String, String)>,
    },
    Denied {
        error: String,
        description: Option<String>,
        uri: Option<String>,
    },
}

impl Callback {
    /// A parameter of a successful callback.
    pub fn param(&self, name: &str) -> Option<&str> {
        match self {
            Self::Code { params, .. } => params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()),
            Self::Denied { .. } => None,
        }
    }
}

/// The pages the browser is shown (whole HTML documents; see [`page`]).
pub trait CallbackPages: Send + Sync {
    /// A request for another path.
    fn not_found(&self, redirect_uri: &str) -> String;
    /// A callback whose `state` is not this sign-in's.
    fn bad_state(&self) -> String;
    /// The user or the server refused (`detail` = `error: description`).
    fn denied(&self, detail: &str) -> String;
    fn success(&self) -> String;
    /// A callback with neither a code nor an error.
    fn no_code(&self) -> String;
}

/// A bound listener on the loopback interface.
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
}

impl CallbackListener {
    pub fn new(listener: TcpListener) -> Self {
        Self { listener }
    }

    /// Binds `127.0.0.1:<port>` (0 = any free port).
    pub async fn bind(port: u16) -> std::io::Result<Self> {
        Ok(Self::new(TcpListener::bind(("127.0.0.1", port)).await?))
    }

    pub fn port(&self) -> std::io::Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    /// Answers the browser until a request to `path` with this `state`
    /// carries a code or an error; anything else gets an explanation.
    /// `None` when the listener fails.
    pub async fn wait(
        self,
        path: &str,
        state: &str,
        redirect_uri: &str,
        pages: &dyn CallbackPages,
    ) -> Option<Callback> {
        loop {
            let Ok((mut socket, _)) = self.listener.accept().await else { return None };
            let mut head = Vec::with_capacity(1024);
            let mut buf = [0u8; 1024];
            while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => head.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            let text = String::from_utf8_lossy(&head);
            let target = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
            let url = reqwest::Url::parse(&format!("http://localhost{target}")).ok();
            let params: Vec<(String, String)> = url
                .as_ref()
                .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
                .unwrap_or_default();
            let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            let (status, html, outcome) = if url.as_ref().is_none_or(|u| u.path() != path) {
                ("404 Not Found", pages.not_found(redirect_uri), None)
            } else if get("state").as_deref() != Some(state) {
                ("400 Bad Request", pages.bad_state(), None)
            } else if let Some(error) = get("error") {
                let description = get("error_description");
                let detail = format!("{error}: {}", description.clone().unwrap_or_default());
                ("200 OK", pages.denied(&detail), Some(Callback::Denied { error, description, uri: get("error_uri") }))
            } else if let Some(code) = get("code") {
                ("200 OK", pages.success(), Some(Callback::Code { code, params: params.clone() }))
            } else {
                ("400 Bad Request", pages.no_code(), None)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            if outcome.is_some() {
                return outcome;
            }
        }
    }
}

/// A plain result page: a green or red heading, a message, optional detail.
pub fn page(ok: bool, heading: &str, message: &str, detail: Option<&str>) -> String {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let color = if ok { "#2e7d32" } else { "#c62828" };
    let detail = detail.map(|d| format!("<pre>{}</pre>", escape(d))).unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{h}</title></head><body style=\"font-family: \
         system-ui; max-width: 40em; margin: 4em auto\"><h1 style=\"color: {color}\">{h}</h1><p>{m}</p>{detail}</body></html>",
        h = escape(heading),
        m = escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;

    impl CallbackPages for Plain {
        fn not_found(&self, redirect_uri: &str) -> String {
            format!("only {redirect_uri}")
        }
        fn bad_state(&self) -> String {
            "bad state".into()
        }
        fn denied(&self, detail: &str) -> String {
            format!("denied {detail}")
        }
        fn success(&self) -> String {
            "ok".into()
        }
        fn no_code(&self) -> String {
            "no code".into()
        }
    }

    async fn get(port: u16, target: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.expect("write");
        let mut out = String::new();
        s.read_to_string(&mut out).await.expect("read");
        out
    }

    #[tokio::test]
    async fn only_the_right_path_and_state_end_the_wait() {
        let listener = CallbackListener::bind(0).await.expect("bind");
        let port = listener.port().expect("port");
        let waiting = tokio::spawn(async move { listener.wait("/auth/callback", "s1", "http://x/cb", &Plain).await });
        assert!(get(port, "/other").await.contains("404 Not Found"));
        assert!(get(port, "/auth/callback?state=nope&code=c").await.contains("bad state"));
        assert!(get(port, "/auth/callback?state=s1&code=c1&client_id=oaiapp_1").await.ends_with("ok"));
        let got = waiting.await.expect("join").expect("callback");
        assert_eq!(got.param("client_id"), Some("oaiapp_1"));
        assert!(matches!(got, Callback::Code { code, .. } if code == "c1"));
    }
}
