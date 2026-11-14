//! Signing in to Claude as `claude auth login` does: an authorization code
//! with PKCE, brought back by the browser to a loopback callback or pasted
//! by the user from the page shown after authorizing, exchanged for tokens,
//! the account's profile read, and all of it saved where Claude Code keeps
//! a sign-in, so either program can use the other's.

use std::future::Future;
use std::time::Duration;

use eos_agent_oauth::{Callback, CallbackListener, CallbackPages, DirLock, now_ms, page, pkce_challenge, random_token};
use serde_json::{Value, json};

use super::{CLIENT_ID, ClaudeLogin, LOCK_POLL, LOCK_STALE, LOCK_WAIT, TOKEN_URL};

const CALLBACK_PATH: &str = "/callback";
/// What Claude Code asks for when signing in with a Claude subscription.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers \
                      user:file_upload user:plugins";

/// Where a sign-in goes; the defaults are Claude's own addresses.
#[derive(Debug, Clone)]
pub struct SignInConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub profile_url: String,
    /// The page that shows the code to paste, for a browser that cannot
    /// reach this machine's loopback address.
    pub manual_redirect_url: String,
    /// Where the browser lands after the loopback callback took the code.
    pub success_url: String,
    /// The loopback port to listen on (0: any free one).
    pub callback_port: u16,
    /// The email to sign in as, pre-filled on the page.
    pub login_hint: Option<String>,
    /// How long to wait for the user.
    pub timeout: Duration,
}

impl Default for SignInConfig {
    fn default() -> Self {
        Self {
            authorize_url: "https://claude.com/cai/oauth/authorize".into(),
            token_url: TOKEN_URL.into(),
            profile_url: "https://api.anthropic.com/api/oauth/profile".into(),
            manual_redirect_url: "https://platform.claude.com/oauth/code/callback".into(),
            success_url: "https://platform.claude.com/oauth/code/success?app=claude-code".into(),
            callback_port: 0,
            login_hint: None,
            timeout: Duration::from_secs(10 * 60),
        }
    }
}

/// Who signed in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeAccount {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub display_name: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
    /// `max`, `pro`, `team` or `enterprise` (what Claude Code stores as
    /// `subscriptionType`); `None` when the profile could not be read.
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    /// What the sign-in may do (`user:inference`, …).
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("the sign-in was declined: {0}")]
    Denied(String),
    #[error("no answer from the browser in time")]
    Timeout,
    #[error("the sign-in failed: {0}")]
    Failed(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn failed(e: impl std::fmt::Display) -> SignInError {
    SignInError::Failed(e.to_string())
}

/// A sign-in under way: show the user one of its addresses, then
/// [`finish`](Self::finish) it.
#[derive(Debug)]
pub struct SignIn {
    config: SignInConfig,
    listener: CallbackListener,
    state: String,
    verifier: String,
    local_redirect: String,
    browser_url: String,
    manual_url: String,
}

impl SignIn {
    /// Prepares a sign-in and starts listening for the browser.
    pub async fn start(config: SignInConfig) -> Result<Self, SignInError> {
        let listener = match CallbackListener::bind(config.callback_port).await {
            Ok(listener) => listener,
            Err(_) => CallbackListener::bind(0).await?,
        };
        let local_redirect = format!("http://localhost:{}{CALLBACK_PATH}", listener.port()?);
        let (state, verifier) = (random_token().map_err(failed)?, random_token().map_err(failed)?);
        let address = |redirect: &str| -> Result<String, SignInError> {
            let mut url = reqwest::Url::parse(&config.authorize_url).map_err(failed)?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("code", "true");
                q.append_pair("client_id", CLIENT_ID);
                q.append_pair("response_type", "code");
                q.append_pair("redirect_uri", redirect);
                q.append_pair("scope", SCOPES);
                q.append_pair("code_challenge", &pkce_challenge(&verifier));
                q.append_pair("code_challenge_method", "S256");
                q.append_pair("state", &state);
                if let Some(hint) = &config.login_hint {
                    q.append_pair("login_hint", hint);
                }
            }
            Ok(url.into())
        };
        let browser_url = address(&local_redirect)?;
        let manual_url = address(&config.manual_redirect_url)?;
        Ok(Self { config, listener, state, verifier, local_redirect, browser_url, manual_url })
    }

    /// The address to open in a browser on this machine: after the user
    /// authorizes, the browser comes back by itself.
    pub fn browser_url(&self) -> &str {
        &self.browser_url
    }

    /// The address for a browser elsewhere: after the user authorizes, the
    /// page shows a code to paste (see [`finish`](Self::finish)).
    pub fn manual_url(&self) -> &str {
        &self.manual_url
    }

    /// Waits for the user to authorize, then signs `login`'s store in.
    /// `pasted` resolves with the code the user pasted from the page of
    /// [`manual_url`](Self::manual_url) (`code#state`), or with `None` when
    /// nobody will paste one; whichever of the browser and the paste comes
    /// first is used.
    pub async fn finish(
        self,
        login: &ClaudeLogin,
        pasted: impl Future<Output = Option<String>>,
    ) -> Result<ClaudeAccount, SignInError> {
        let Self { config, listener, state, verifier, local_redirect, .. } = self;
        let pages = Pages { success_url: config.success_url.clone() };
        let pasted = async {
            match pasted.await {
                Some(text) => text,
                None => std::future::pending().await,
            }
        };
        let (code, redirect) = tokio::select! {
            callback = listener.wait(CALLBACK_PATH, &state, &local_redirect, &pages) => match callback {
                Some(Callback::Code { code, .. }) => (code, local_redirect.as_str()),
                Some(Callback::Denied { error, description, .. }) => {
                    let detail = description.map(|d| format!(": {d}")).unwrap_or_default();
                    return Err(SignInError::Denied(format!("{error}{detail}")));
                }
                None => return Err(failed("the callback listener stopped")),
            },
            text = pasted => match text.trim().split_once('#') {
                Some((code, from)) if !code.is_empty() && from == state => {
                    (code.to_owned(), config.manual_redirect_url.as_str())
                }
                Some(_) => return Err(failed("that code is not from this sign-in")),
                None => return Err(failed("the code is incomplete: paste all of it")),
            },
            () = tokio::time::sleep(config.timeout) => return Err(SignInError::Timeout),
        };

        let http = &login.http;
        let exchange = json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        });
        let res = http
            .post(&config.token_url)
            .header("content-type", "application/json")
            .body(exchange.to_string())
            .send()
            .await
            .map_err(failed)?;
        let status = res.status();
        let tokens = body_json(res).await;
        if !status.is_success() {
            return Err(failed(match status.as_u16() {
                401 => "invalid authorization code".to_owned(),
                code => format!("the token exchange was refused ({code})"),
            }));
        }
        let access = tokens["access_token"].as_str().ok_or_else(|| failed("the exchange gave no access token"))?;
        let refresh = tokens["refresh_token"].as_str().ok_or_else(|| failed("the exchange gave no refresh token"))?;
        let expires_at = now_ms() + tokens["expires_in"].as_u64().unwrap_or(0) * 1000;
        let scopes: Vec<String> = tokens["scope"]
            .as_str()
            .unwrap_or(SCOPES)
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();

        // The plan comes from the profile; without it the sign-in still works.
        let profile: Value = match http.get(&config.profile_url).bearer_auth(access).send().await {
            Ok(res) if res.status().is_success() => body_json(res).await,
            _ => Value::Null,
        };
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let (who, org) = (&profile["account"], &profile["organization"]);
        let account = ClaudeAccount {
            email: text(&who["email"]).or_else(|| text(&tokens["account"]["email_address"])),
            account_uuid: text(&who["uuid"]).or_else(|| text(&tokens["account"]["uuid"])),
            display_name: text(&who["display_name"]),
            organization_uuid: text(&org["uuid"]).or_else(|| text(&tokens["organization"]["uuid"])),
            organization_name: text(&org["name"]).or_else(|| text(&tokens["organization"]["name"])),
            subscription_type: org["organization_type"].as_str().and_then(subscription_type).map(str::to_owned),
            rate_limit_tier: text(&org["rate_limit_tier"]),
            scopes,
        };

        let sign_in = json!({
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
            "scopes": account.scopes,
            "subscriptionType": account.subscription_type,
            "rateLimitTier": account.rate_limit_tier,
        });
        login.store(Some(sign_in)).await?;
        Ok(account)
    }
}

/// The answer's body as JSON (`null` when it is not).
async fn body_json(res: reqwest::Response) -> Value {
    res.text().await.ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null)
}

/// Claude Code's name for a plan, from the profile's organization type.
fn subscription_type(organization_type: &str) -> Option<&'static str> {
    Some(match organization_type {
        "claude_max" => "max",
        "claude_pro" => "pro",
        "claude_enterprise" => "enterprise",
        "claude_team" => "team",
        _ => return None,
    })
}

impl ClaudeLogin {
    /// Signs the store out: its Claude sign-in is forgotten (other things
    /// kept in the same place, such as MCP sign-ins, stay). False when it
    /// was not signed in.
    pub async fn sign_out(&self) -> std::io::Result<bool> {
        self.store(None).await
    }

    /// Puts `sign_in` in the store's document as `claudeAiOauth` (or takes
    /// it out), under the lock renewals take. True when the document changed.
    async fn store(&self, sign_in: Option<Value>) -> std::io::Result<bool> {
        let _lock = DirLock::wait_for(&self.lock_dir, LOCK_STALE, LOCK_POLL, LOCK_WAIT)
            .await?
            .ok_or_else(|| std::io::Error::other("another process holds the sign-in lock"))?;
        let raw = self.secrets.read().await?;
        let mut doc: Value = raw.as_deref().and_then(|raw| serde_json::from_str(raw).ok()).unwrap_or_else(|| json!({}));
        let Some(members) = doc.as_object_mut() else {
            return Err(std::io::Error::other("the stored sign-in is not a JSON object"));
        };
        let changed = match sign_in {
            Some(sign_in) => {
                members.insert("claudeAiOauth".into(), sign_in);
                true
            }
            None => members.remove("claudeAiOauth").is_some(),
        };
        if changed {
            self.secrets.write(&doc.to_string()).await?;
        }
        *self.cached.lock().await = None;
        Ok(changed)
    }
}

/// What the browser is shown at the loopback callback.
struct Pages {
    success_url: String,
}

impl CallbackPages for Pages {
    fn not_found(&self, redirect_uri: &str) -> String {
        page(false, "Not found", &format!("Only {redirect_uri} is served here."), None)
    }

    fn bad_state(&self) -> String {
        page(false, "Sign-in refused", "This answer does not belong to the sign-in that is waiting.", None)
    }

    fn denied(&self, detail: &str) -> String {
        page(false, "Sign-in declined", "Claude was not signed in.", Some(detail))
    }

    /// Claude Code sends the browser on to Claude's own "signed in" page.
    fn success(&self) -> String {
        let url = self.success_url.replace('&', "&amp;").replace('"', "&quot;");
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" \
             content=\"0;url={url}\"><title>Signed in</title></head><body><p>Signed in. You can close this \
             tab.</p></body></html>"
        )
    }

    fn no_code(&self) -> String {
        page(false, "Sign-in failed", "The answer carried no authorization code.", None)
    }
}
