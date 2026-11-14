//! The browser sign-in: authorization code with PKCE, a loopback callback
//! on 127.0.0.1, the code exchanged for tokens, the ID token checked
//! against the issuer's keys, everything saved to the store directory.

use std::path::Path;

use eos_agent_oauth::{
    Callback, CallbackListener, CallbackPages, DirLock, FileSecrets, now_ms, page, pkce_challenge, post_form,
    random_token, verify_rs256,
};
use serde_json::Value;

use super::{Account, HOST_ID, LOCK, LOCK_POLL, LOCK_STALE, LOCK_WAIT, RECORD, SignInConfig, host_id, read, write};

/// The client id of a first sign-in; OpenAI issues the app its own then.
const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// How the user reaches the sign-in page: a host shows `url` (opens a
/// browser, prints it, sends it to its UI). Must not block.
pub trait BrowserOpener: Send + Sync {
    fn open(&self, url: &str);
}

/// Prints the address to stderr and, when asked, opens it in the default browser.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrintOpener {
    pub open_browser: bool,
}

impl BrowserOpener for PrintOpener {
    fn open(&self, url: &str) {
        eprintln!("\nSign in with ChatGPT in your browser. If it does not open, visit:\n{url}\n");
        if self.open_browser {
            let _ = std::process::Command::new("open")
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("the sign-in was declined: {0}")]
    Denied(String),
    #[error("no answer from the browser in time")]
    Timeout,
    #[error("the sign-in failed: {0}")]
    Failed(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn failed(e: impl std::fmt::Display) -> LoginError {
    LoginError::Failed(e.to_string())
}

/// Signs `store_dir` in to ChatGPT through the user's browser: a first
/// sign-in registers this app for the account; later ones reuse the
/// registration and must be the same account.
pub async fn sign_in(
    store_dir: &Path,
    config: &SignInConfig,
    opener: &dyn BrowserOpener,
) -> Result<Account, LoginError> {
    tokio::fs::create_dir_all(store_dir).await?;
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(store_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let secrets = FileSecrets::new(store_dir.join(RECORD));
    let saved = read(&secrets).await?;
    let host = host_id(store_dir).await?;
    let listener = match CallbackListener::bind(config.callback_port).await {
        Ok(l) => l,
        // Only the port may vary between sign-ins.
        Err(_) => CallbackListener::bind(0).await?,
    };
    let redirect = format!("http://127.0.0.1:{}{CALLBACK_PATH}", listener.port()?);
    let (state, nonce, verifier) =
        (random_token().map_err(failed)?, random_token().map_err(failed)?, random_token().map_err(failed)?);
    let client = saved.as_ref().map_or(REGISTRATION_CLIENT, |r| r.client_id.as_str());

    let mut url = reqwest::Url::parse(&config.authorize_url).map_err(failed)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client);
        q.append_pair("redirect_uri", &redirect);
        q.append_pair("scope", SCOPES);
        q.append_pair("resource", &config.resource);
        q.append_pair("state", &state);
        q.append_pair("nonce", &nonce);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("code_challenge", &pkce_challenge(&verifier));
        q.append_pair("ext_agent_host_id", &host);
        match &saved {
            None => {
                q.append_pair("agent_name_hint", &config.app_name);
            }
            Some(r) => {
                if let Some(hint) = &r.id_token {
                    q.append_pair("id_token_hint", hint);
                }
                if let Some(email) = &r.email {
                    q.append_pair("login_hint", email);
                }
            }
        }
    }
    opener.open(url.as_str());

    let pages = Pages(config.app_name.clone());
    let callback = tokio::time::timeout(config.timeout, listener.wait(CALLBACK_PATH, &state, &redirect, &pages))
        .await
        .map_err(|_| LoginError::Timeout)?
        .ok_or_else(|| failed("the callback listener stopped"))?;
    let code = match &callback {
        Callback::Denied { error, description, .. } => {
            return Err(LoginError::Denied(format!(
                "{error}{}",
                description.as_deref().map(|d| format!(": {d}")).unwrap_or_default()
            )));
        }
        Callback::Code { code, .. } => code.clone(),
    };
    let client_id = match (callback.param("client_id"), &saved) {
        (Some(issued), Some(r)) if issued != r.client_id => {
            return Err(failed("the callback names another app registration than this store's; sign-in refused"));
        }
        (_, Some(r)) => r.client_id.clone(),
        (Some(issued), None) if issued != REGISTRATION_CLIENT => issued.to_owned(),
        (_, None) => return Err(failed("the registration did not finish: no client id came back")),
    };

    let http = reqwest::Client::new();
    let pairs = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("resource", config.resource.as_str()),
    ];
    let answer = post_form(&http, &config.token_url, &pairs).await.map_err(failed)?;
    let body = answer.body.clone().unwrap_or_default();
    if answer.status != 200 || !body["access_token"].is_string() {
        return Err(failed(format!(
            "the code exchange was refused (HTTP {}, {})",
            answer.status,
            answer.error_code().unwrap_or("no error code")
        )));
    }
    let id_token = body["id_token"].as_str().ok_or_else(|| failed("no ID token came back"))?;
    let jwks = jwks(&http, config).await?;
    let claims = verify_rs256(id_token, &jwks).map_err(failed)?;
    check_claims(&claims, config, &client_id, &nonce)?;
    let subject = claims["sub"].as_str().unwrap_or_default().to_owned();
    if let Some(r) = &saved
        && !r.subject.is_empty()
        && r.subject != subject
    {
        return Err(failed("signed in as another ChatGPT account than this store's; use another store directory"));
    }

    let mut record = saved.unwrap_or_default();
    record.issuer.clone_from(&config.issuer);
    record.subject = subject;
    record.email = claims["email"].as_str().map(str::to_owned).or(record.email);
    record.client_id = client_id;
    record.ext_agent_host_id = host;
    record.apply(&body);
    record.id_token = Some(id_token.to_owned());
    if body["scope"].as_str().is_none() {
        // The callback's scope is all we were told.
        if let Some(scope) = callback.param("scope") {
            record.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
    }
    let _held = DirLock::wait_for(&store_dir.join(LOCK), LOCK_STALE, LOCK_POLL, LOCK_WAIT)
        .await?
        .ok_or_else(|| failed("another process holds the sign-in lock"))?;
    write(&secrets, &record).await.map_err(failed)?;
    debug_assert!(store_dir.join(HOST_ID).is_file());
    Ok(Account {
        email: record.email.clone(),
        subject: record.subject.clone(),
        client_id: record.client_id.clone(),
        plan_usage: record.can_use_plan(),
    })
}

/// The issuer's signing keys, from its OpenID configuration unless set.
async fn jwks(http: &reqwest::Client, config: &SignInConfig) -> Result<Value, LoginError> {
    let get = |url: String| async move {
        let res = http.get(&url).send().await.map_err(failed)?;
        if !res.status().is_success() {
            return Err(failed(format!("{url}: HTTP {}", res.status())));
        }
        let text = res.text().await.map_err(failed)?;
        serde_json::from_str::<Value>(&text).map_err(failed)
    };
    let url = match &config.jwks_url {
        Some(url) => url.clone(),
        None => {
            let discovery =
                get(format!("{}/.well-known/openid-configuration", config.issuer.trim_end_matches('/'))).await?;
            discovery["jwks_uri"].as_str().ok_or_else(|| failed("the issuer names no signing keys"))?.to_owned()
        }
    };
    get(url).await
}

fn check_claims(claims: &Value, config: &SignInConfig, client_id: &str, nonce: &str) -> Result<(), LoginError> {
    let iss = claims["iss"].as_str().unwrap_or_default();
    if iss.trim_end_matches('/') != config.issuer.trim_end_matches('/') {
        return Err(failed(format!("the ID token comes from another issuer ({iss})")));
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(auds) => auds.iter().any(|a| a == client_id),
        _ => false,
    };
    if !audience_ok {
        return Err(failed("the ID token is for another client"));
    }
    if claims["exp"].as_u64().is_none_or(|exp| exp * 1000 <= now_ms()) {
        return Err(failed("the ID token has expired"));
    }
    if claims["nonce"].as_str() != Some(nonce) {
        return Err(failed("the ID token does not answer this sign-in (nonce)"));
    }
    if claims["sub"].as_str().is_none_or(str::is_empty) {
        return Err(failed("the ID token names no account"));
    }
    Ok(())
}

struct Pages(String);

impl CallbackPages for Pages {
    fn not_found(&self, redirect_uri: &str) -> String {
        page(false, "Not found", &format!("This listener only answers {redirect_uri}."), None)
    }

    fn bad_state(&self) -> String {
        page(false, "Sign-in failed", "This answer belongs to another sign-in. Close this tab and try again.", None)
    }

    fn denied(&self, detail: &str) -> String {
        page(false, "Sign-in declined", "Close this tab; you can sign in again at any time.", Some(detail))
    }

    fn success(&self) -> String {
        page(true, "Signed in", &format!("You can close this tab and return to {}.", self.0), None)
    }

    fn no_code(&self) -> String {
        page(false, "Sign-in failed", "No authorization code came back.", None)
    }
}
