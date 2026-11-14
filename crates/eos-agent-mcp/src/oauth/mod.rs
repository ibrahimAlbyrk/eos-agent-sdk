//! OAuth for remote MCP servers, as Claude Code 2.1.319 does it: a server
//! that answers 401 is signed in to through discovery (RFC 9728, RFC 8414),
//! a client registration (configured, a client ID metadata document, or
//! dynamic registration, RFC 7591), PKCE and a localhost callback; tokens
//! are kept in Claude Code's credentials store and renewed under its lock.

mod callback;
mod client;
mod connect;
mod discovery;
mod flow;
mod needs_auth;
mod plan;
mod provider;
mod revoke;
mod store;
mod token;
mod tools;
mod xaa;
mod xaa_idp;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::BoxFuture;

pub(crate) use client::AuthedClient;
pub use eos_agent_oauth::MemorySecrets;
pub(crate) use flow::ActiveFlow;
pub(crate) use provider::{Rejection, ServerAuth};
pub use store::McpCredentials;
pub(crate) use tools::auth_tools;
pub use xaa_idp::XaaIdp;

/// A sign-in that needs the user: the host shows them `url` (opens a browser,
/// prints it, sends it to its UI). The flow waits for the browser to come
/// back to `redirect_uri` (or for the address to be pasted to the
/// `complete_authentication` tool) for up to five minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    /// The MCP server's name in the config.
    pub server: String,
    pub url: String,
    pub redirect_uri: String,
    /// The agent whose `authenticate` tool call started the sign-in.
    pub agent_id: Option<String>,
    /// Started by "Re-authenticate in browser" on the step-up question:
    /// Claude Code then opens the browser itself and says so (a sign-in the
    /// `authenticate` tool starts only hands the address to the model).
    pub step_up: bool,
}

/// How a host learns that the user must authorize a server. Called once per
/// sign-in; must not block (start the work and return).
pub trait AuthPrompter: Send + Sync {
    fn authorize(&self, request: &AuthRequest);

    /// Claude Code's step-up question: a tool call was refused for want of a
    /// wider scope; may the server be signed in to again (in the browser)
    /// so the call can be retried? Asked only for a top-level agent's call.
    /// The default asks nobody ([`StepUpAnswer::Unanswered`]): the call fails
    /// and the server waits for a sign-in, as in Claude Code's `-p`.
    fn ask_step_up<'a>(&'a self, _request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        Box::pin(async { StepUpAnswer::Unanswered })
    }
}

/// The question Claude Code puts to the user when a tool call needs a wider
/// scope, with its two choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpRequest {
    pub server: String,
    /// The scope the server asked for.
    pub scope: String,
    /// Claude Code's question text.
    pub question: String,
    pub choices: Vec<StepUpChoice>,
    /// The agent whose tool call was refused.
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpChoice {
    /// `reauthenticate` or `not_now`.
    pub value: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// Claude Code's choices, in its order and words.
pub const STEP_UP_CHOICES: [StepUpChoice; 2] = [
    StepUpChoice {
        value: "reauthenticate",
        label: "Re-authenticate in browser",
        description: "Signs this server out until you finish authorizing in the browser (for the scope above plus what \
                      it already has); if you do not finish, run /mcp to sign back in. The tool call is retried once \
                      afterwards",
    },
    StepUpChoice {
        value: "not_now",
        label: "Not now",
        description: "The tool call fails with the permission error; other tools keep working",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepUpAnswer {
    /// Sign in again, then retry the call.
    Reauthenticate,
    NotNow,
    /// Nobody could be asked, or nobody answered.
    Unanswered,
}

/// Opens an address in the user's browser.
pub type BrowserOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// The default: shows nothing for a sign-in the `authenticate` tool starts
/// (Claude Code hands its address to the model only, and prints nothing),
/// and asks the step-up question on the terminal (`/dev/tty`) when there is
/// one, as Claude Code's interactive terminal does; on "Re-authenticate in
/// browser" it opens the browser and says so in Claude Code's words (on
/// stderr, or a given writer). [`without_questions`](Self::without_questions)
/// asks nobody, as Claude Code's `-p`.
pub struct PrintPrompter {
    out: Mutex<Box<dyn Write + Send>>,
    /// Where the step-up question is asked; `None`: the controlling terminal.
    tty: Option<PathBuf>,
    asks: bool,
    open: BrowserOpener,
}

impl std::fmt::Debug for PrintPrompter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrintPrompter")
    }
}

impl Default for PrintPrompter {
    fn default() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }
}

impl PrintPrompter {
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out: Mutex::new(out), tty: None, asks: true, open: Arc::new(open_in_browser) }
    }

    /// Asks the step-up question on this terminal device (or file) instead
    /// of `/dev/tty`.
    #[must_use]
    pub fn with_tty(mut self, path: impl Into<PathBuf>) -> Self {
        self.tty = Some(path.into());
        self
    }

    /// Never asks the step-up question (Claude Code's `-p`: nobody can be
    /// asked, so the call fails and the server waits for a sign-in).
    #[must_use]
    pub fn without_questions(mut self) -> Self {
        self.asks = false;
        self
    }

    /// Opens the browser this way (default: macOS `open`).
    #[must_use]
    pub fn with_browser(mut self, open: BrowserOpener) -> Self {
        self.open = open;
        self
    }
}

impl AuthPrompter for PrintPrompter {
    fn authorize(&self, request: &AuthRequest) {
        if !request.step_up {
            return;
        }
        (self.open)(&request.url);
        let text = format!("\n{}\n", step_up_browser_notice(&request.server, &request.url));
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    fn ask_step_up<'a>(&'a self, request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        if !self.asks {
            return Box::pin(async { StepUpAnswer::Unanswered });
        }
        let tty = self.tty.clone().unwrap_or_else(|| PathBuf::from("/dev/tty"));
        let request = request.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || ask_on_terminal(&tty, &request))
                .await
                .unwrap_or(StepUpAnswer::Unanswered)
        })
    }
}

/// What Claude Code shows when the browser opens for a step-up sign-in: the
/// link itself when it can be shown as it is, else a pointer to `/mcp`.
pub fn step_up_browser_notice(server: &str, url: &str) -> String {
    let lead = format!("Opening your browser to re-authenticate MCP server \"{server}\".");
    if url.chars().count() <= 8192 && !url.chars().any(hidden) {
        format!("{lead} If it doesn't open, use this link to the server's sign-in page: {url}")
    } else {
        format!("{lead} If it doesn't open, run /mcp to sign back in; its sign-in screen shows the link.")
    }
}

/// Characters Claude Code's display sanitizer removes (controls, format and
/// bidi marks), so a link holding one is not shown.
fn hidden(c: char) -> bool {
    c.is_control()
        || ('\u{200b}'..='\u{200f}').contains(&c)
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
        || c == '\u{feff}'
}

fn open_in_browser(url: &str) {
    let _ = std::process::Command::new("open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The question and its numbered choices on the terminal; a line naming a
/// choice answers it.
fn ask_on_terminal(tty: &std::path::Path, request: &StepUpRequest) -> StepUpAnswer {
    use std::io::BufRead as _;
    let (Ok(mut out), Ok(file)) = (std::fs::OpenOptions::new().append(true).open(tty), std::fs::File::open(tty)) else {
        return StepUpAnswer::Unanswered;
    };
    let mut text = format!("\nMCP · {}\n", request.question);
    for (i, c) in request.choices.iter().enumerate() {
        text.push_str(&format!("  {}. {} — {}\n", i + 1, c.label, c.description));
    }
    text.push_str("Choose 1 or 2: ");
    if out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_err() {
        return StepUpAnswer::Unanswered;
    }
    let mut line = String::new();
    if std::io::BufReader::new(file).read_line(&mut line).is_err() {
        return StepUpAnswer::Unanswered;
    }
    let pick = line.trim();
    let chosen = request.choices.iter().enumerate().find(|(i, c)| {
        pick == (i + 1).to_string() || pick.eq_ignore_ascii_case(c.value) || pick.eq_ignore_ascii_case(c.label)
    });
    match chosen.map(|(_, c)| c.value) {
        Some("reauthenticate") => StepUpAnswer::Reauthenticate,
        Some(_) => StepUpAnswer::NotNow,
        None => StepUpAnswer::Unanswered,
    }
}

/// What every server's OAuth shares in one hub.
pub(crate) struct AuthContext {
    pub(crate) credentials: McpCredentials,
    pub(crate) prompter: Arc<dyn AuthPrompter>,
    pub(crate) http: reqwest::Client,
    /// Claude Code's `xaaIdp` setting.
    pub(crate) xaa_idp: Option<XaaIdp>,
    xaa_enabled: Option<bool>,
}

/// More of what MCP OAuth may be given.
#[derive(Debug, Clone, Default)]
pub struct OAuthOptions {
    /// Claude Code's `xaaIdp` setting: the IdP for servers with `oauth.xaa`.
    pub xaa_idp: Option<XaaIdp>,
    /// The HTTP client for every OAuth request (default: a plain one).
    pub http: Option<reqwest::Client>,
    /// Cross-app access on or off (default: Claude Code's
    /// `CLAUDE_CODE_ENABLE_XAA` environment variable).
    pub xaa_enabled: Option<bool>,
}

impl AuthContext {
    pub(crate) fn new(credentials: McpCredentials, prompter: Arc<dyn AuthPrompter>, options: OAuthOptions) -> Self {
        Self {
            credentials,
            prompter,
            http: options.http.unwrap_or_default(),
            xaa_idp: options.xaa_idp,
            xaa_enabled: options.xaa_enabled,
        }
    }

    /// Claude Code's `CLAUDE_CODE_ENABLE_XAA`: cross-app access for servers with `oauth.xaa`.
    pub(crate) fn xaa_enabled(&self) -> bool {
        self.xaa_enabled.unwrap_or_else(|| {
            std::env::var("CLAUDE_CODE_ENABLE_XAA")
                .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        })
    }
}

pub(crate) use eos_agent_oauth::{now_ms, random_token};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_default_prompter_asks_on_the_terminal() {
        let tty = std::env::temp_dir().join(format!("eos-step-up-tty-{}", std::process::id()));
        std::fs::write(&tty, "1\n").expect("tty");
        let prompter = PrintPrompter::default().with_tty(&tty);
        let request = StepUpRequest {
            server: "docs".into(),
            scope: "admin".into(),
            question: "Re-authenticate now?".into(),
            choices: STEP_UP_CHOICES.to_vec(),
            agent_id: None,
        };
        assert_eq!(prompter.ask_step_up(&request).await, StepUpAnswer::Reauthenticate);
        let shown = std::fs::read_to_string(&tty).expect("read");
        assert!(shown.contains("MCP · Re-authenticate now?\n  1. Re-authenticate in browser — "), "{shown}");
        std::fs::write(&tty, "2\n").expect("tty");
        assert_eq!(prompter.ask_step_up(&request).await, StepUpAnswer::NotNow);
        let nowhere = PrintPrompter::default().with_tty("/nonexistent/tty");
        assert_eq!(nowhere.ask_step_up(&request).await, StepUpAnswer::Unanswered);
        // As `-p`: nobody is asked, even with a terminal at hand.
        std::fs::write(&tty, "1\n").expect("tty");
        let silent = PrintPrompter::default().with_tty(&tty).without_questions();
        assert_eq!(silent.ask_step_up(&request).await, StepUpAnswer::Unanswered);
        assert_eq!(std::fs::read_to_string(&tty).expect("read"), "1\n");
        let _ = std::fs::remove_file(&tty);
    }

    #[test]
    fn a_step_up_sign_in_opens_the_browser_and_says_so() {
        let buf = Buf::default();
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&opened);
        let prompter = PrintPrompter::new(Box::new(buf.clone()))
            .with_browser(Arc::new(move |url: &str| seen.lock().expect("lock").push(url.to_owned())));
        prompter.authorize(&AuthRequest {
            server: "docs".into(),
            url: "https://as.dev/authorize?x=1".into(),
            redirect_uri: "http://localhost:1/callback".into(),
            agent_id: None,
            step_up: true,
        });
        assert_eq!(*opened.lock().expect("lock"), ["https://as.dev/authorize?x=1"]);
        let text = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert_eq!(
            text,
            "\nOpening your browser to re-authenticate MCP server \"docs\". If it doesn't open, use this link to the \
             server's sign-in page: https://as.dev/authorize?x=1\n"
        );
        assert_eq!(
            step_up_browser_notice("docs", "https://as.dev/a\u{202e}b"),
            "Opening your browser to re-authenticate MCP server \"docs\". If it doesn't open, run /mcp to sign back \
             in; its sign-in screen shows the link."
        );
    }

    #[test]
    fn the_authenticate_tool_sign_in_prints_nothing() {
        let buf = Buf::default();
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&opened);
        let prompter = PrintPrompter::new(Box::new(buf.clone()))
            .with_browser(Arc::new(move |url: &str| seen.lock().expect("lock").push(url.to_owned())));
        prompter.authorize(&AuthRequest {
            server: "github".into(),
            url: "https://as.dev/authorize?x=1".into(),
            redirect_uri: "http://localhost:1/callback".into(),
            agent_id: None,
            step_up: false,
        });
        assert!(buf.0.lock().expect("lock").is_empty(), "the address goes to the model only");
        assert!(opened.lock().expect("lock").is_empty(), "no browser either (`skipBrowserOpen`)");
    }
}
