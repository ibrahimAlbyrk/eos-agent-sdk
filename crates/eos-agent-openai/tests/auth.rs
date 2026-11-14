//! Sign-in and renewal against a fake OpenAI auth server: PKCE, the
//! registration's client id, ID-token checks, and renewals that spend each
//! refresh token once across processes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Form;
use axum::extract::State;
use axum::routing::{get, post};
use eos_agent_core::{Credential, ModelError, TokenSource};
use eos_agent_oauth::testing::{sign, test_key};
use eos_agent_openai::{BrowserOpener, ChatGptLogin, SignInConfig, sign_in};
use serde_json::{Value, json};

#[derive(Default)]
struct Auth {
    /// The refresh token that still works (single use: each refresh replaces it).
    live_refresh: Mutex<String>,
    refreshes: AtomicU32,
    /// The nonce the next ID token carries (`None`: the one the request sent).
    nonce: Mutex<Option<String>>,
    last_nonce: Mutex<String>,
    forms: Mutex<Vec<Vec<(String, String)>>>,
}

struct Server {
    base: String,
    state: Arc<Auth>,
    _task: tokio::task::JoinHandle<()>,
}

async fn server() -> Server {
    let state = Arc::new(Auth::default());
    let (key, jwk) = test_key();
    let signer = Arc::new(key);
    let app = axum::Router::new()
        .route("/jwks", get(move || async move { axum::Json(json!({"keys": [jwk]})) }))
        .route(
            "/token",
            post(move |State(s): State<Arc<Auth>>, Form(form): Form<Vec<(String, String)>>| {
                let signer = Arc::clone(&signer);
                async move {
                    let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
                    s.forms.lock().expect("lock").push(form.clone());
                    let reply = |status: u16, body: Value| {
                        (axum::http::StatusCode::from_u16(status).expect("status"), axum::Json(body))
                    };
                    if get("grant_type") == "refresh_token" {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let mut live = s.live_refresh.lock().expect("lock");
                        if get("refresh_token") != *live {
                            return reply(400, json!({"error": "refresh_token_reused"}));
                        }
                        let n = s.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                        *live = format!("r{n}");
                        return reply(
                            200,
                            json!({"access_token": format!("a{n}"), "refresh_token": format!("r{n}"),
                            "expires_in": 3600, "token_type": "Bearer"}),
                        );
                    }
                    let nonce = s
                        .nonce
                        .lock()
                        .expect("lock")
                        .clone()
                        .unwrap_or_else(|| s.last_nonce.lock().expect("lock").clone());
                    let exp =
                        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs()
                            + 600;
                    let id_token = sign(
                        &signer,
                        &json!({"iss": "https://auth.test", "aud": get("client_id"),
                        "sub": "user-1", "email": "me@example.com", "exp": exp, "nonce": nonce}),
                    );
                    *s.live_refresh.lock().expect("lock") = "r0".into();
                    reply(
                        200,
                        json!({"access_token": "a0", "refresh_token": "r0", "id_token": id_token, "expires_in": 3600,
                        "scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"}),
                    )
                }
            }),
        )
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Server { base, state, _task: task }
}

fn config(s: &Server) -> SignInConfig {
    SignInConfig {
        issuer: "https://auth.test".into(),
        authorize_url: "https://auth.test/api/accounts/authorize".into(),
        token_url: format!("{}/token", s.base),
        jwks_url: Some(format!("{}/jwks", s.base)),
        callback_port: 0,
        timeout: Duration::from_secs(10),
        ..SignInConfig::default()
    }
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("eos-openai-auth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Plays the browser: checks the authorization request and comes back to
/// the callback with a code (and, on a first sign-in, the issued client id).
struct Browser {
    state: Arc<Auth>,
    seen: Mutex<Vec<String>>,
    issue: Option<&'static str>,
}

impl BrowserOpener for Browser {
    fn open(&self, url: &str) {
        self.seen.lock().expect("lock").push(url.to_owned());
        let url = reqwest::Url::parse(url).expect("url");
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
        *self.state.last_nonce.lock().expect("lock") = q("nonce");
        let mut back = format!("{}?code=c1&state={}", q("redirect_uri"), q("state"));
        if let Some(id) = self.issue {
            back.push_str(&format!("&client_id={id}"));
        }
        tokio::spawn(async move {
            let _ = reqwest::get(back).await;
        });
    }
}

fn query(url: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(url).expect("url").query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn param<'a>(pairs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn first_sign_in_registers_and_later_ones_reuse_the_registration() {
    let s = server().await;
    let store = dir("first");
    let browser = Browser { state: Arc::clone(&s.state), seen: Mutex::default(), issue: Some("oaiapp_1") };
    let account = sign_in(&store, &config(&s), &browser).await.expect("signed in");
    assert_eq!(account.subject, "user-1");
    assert_eq!(account.email.as_deref(), Some("me@example.com"));
    assert_eq!(account.client_id, "oaiapp_1");
    assert!(account.plan_usage);

    let first = query(&browser.seen.lock().expect("lock")[0]);
    assert_eq!(param(&first, "client_id"), Some("dynamic_agent_client"));
    assert_eq!(param(&first, "agent_name_hint"), Some("Eos"));
    assert_eq!(param(&first, "code_challenge_method"), Some("S256"));
    assert_eq!(param(&first, "resource"), Some("https://api.openai.com/v1"));
    assert!(param(&first, "scope").is_some_and(|s| s.contains("chatgpt.tokens.use.direct")));
    assert!(
        param(&first, "redirect_uri")
            .is_some_and(|r| r.starts_with("http://127.0.0.1:") && r.ends_with("/auth/callback"))
    );
    let host = param(&first, "ext_agent_host_id").expect("host id").to_owned();
    assert!(host.starts_with("urn:uuid:"));
    let exchange = s.state.forms.lock().expect("lock")[0].clone();
    assert_eq!(param(&exchange, "client_id"), Some("oaiapp_1"), "the issued id, not dynamic_agent_client");
    assert!(param(&exchange, "code_verifier").is_some());

    let record = store.join("siwc.json");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&record).expect("saved").permissions().mode() & 0o777, 0o600);

    // A second sign-in: the issued client id, the account's hints, the same host id.
    let again = Browser { state: Arc::clone(&s.state), seen: Mutex::default(), issue: None };
    sign_in(&store, &config(&s), &again).await.expect("signed in again");
    let second = query(&again.seen.lock().expect("lock")[0]);
    assert_eq!(param(&second, "client_id"), Some("oaiapp_1"));
    assert!(param(&second, "agent_name_hint").is_none());
    assert!(param(&second, "id_token_hint").is_some());
    assert_eq!(param(&second, "login_hint"), Some("me@example.com"));
    assert_eq!(param(&second, "ext_agent_host_id"), Some(host.as_str()));

    // A callback naming another registration is refused.
    let other = Browser { state: Arc::clone(&s.state), seen: Mutex::default(), issue: Some("oaiapp_2") };
    assert!(sign_in(&store, &config(&s), &other).await.is_err());
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn an_id_token_for_another_sign_in_is_refused() {
    let s = server().await;
    let store = dir("nonce");
    *s.state.nonce.lock().expect("lock") = Some("someone-elses".into());
    let browser = Browser { state: Arc::clone(&s.state), seen: Mutex::default(), issue: Some("oaiapp_1") };
    let err = sign_in(&store, &config(&s), &browser).await.expect_err("refused");
    assert!(err.to_string().contains("nonce"), "{err}");
    assert!(!ChatGptLogin::exists(&store), "nothing saved");
    let _ = std::fs::remove_dir_all(&store);
}

/// A store signed in with an access token that has already lapsed.
fn expired_store(name: &str, refresh: &str) -> PathBuf {
    let store = dir(name);
    std::fs::create_dir_all(&store).expect("dir");
    let record = json!({"issuer": "https://auth.test", "subject": "user-1", "client_id": "oaiapp_1",
        "ext_agent_host_id": "urn:uuid:x", "access_token": "stale", "refresh_token": refresh, "expires_at": 1,
        "scopes": ["chatgpt.tokens.use.direct"], "saved_at": 1});
    std::fs::write(store.join("siwc.json"), record.to_string()).expect("write");
    store
}

#[tokio::test]
async fn two_processes_renew_once_and_both_get_the_new_token() {
    let s = server().await;
    *s.state.live_refresh.lock().expect("lock") = "r0".into();
    let store = expired_store("race", "r0");
    // Two logins over one store stand for two processes: each has its own cache.
    let a = ChatGptLogin::with_config(&store, config(&s));
    let b = ChatGptLogin::with_config(&store, config(&s));
    let (ta, tb) = tokio::join!(a.credential(), b.credential());
    assert_eq!(s.state.refreshes.load(Ordering::SeqCst), 1, "one refresh for both");
    assert_eq!(ta.expect("a"), Credential::Bearer("a1".into()));
    assert_eq!(tb.expect("b"), Credential::Bearer("a1".into()));
    let refresh = s.state.forms.lock().expect("lock")[0].clone();
    assert_eq!(param(&refresh, "client_id"), Some("oaiapp_1"));
    assert_eq!(param(&refresh, "resource"), Some("https://api.openai.com/v1"));
    assert!(param(&refresh, "scope").is_none(), "the grant is kept as it is");

    // The server refused the fresh token: renewed once more.
    assert!(a.refused(&Credential::Bearer("a1".into())).await);
    assert_eq!(a.credential().await.expect("renewed"), Credential::Bearer("a2".into()));
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn a_spent_refresh_token_ends_the_sign_in() {
    let s = server().await;
    *s.state.live_refresh.lock().expect("lock") = "r9".into();
    let store = expired_store("spent", "r0");
    let login = ChatGptLogin::with_config(&store, config(&s));
    let err = login.credential().await.expect_err("ended");
    assert!(
        matches!(&err, ModelError::Auth(m) if m.contains("refresh_token_reused") && m.contains("login chatgpt")),
        "{err}"
    );
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(store.join("siwc.json")).expect("read")).expect("json");
    assert!(saved.get("access_token").is_none() && saved.get("refresh_token").is_none(), "tokens cleared");
    assert_eq!(saved["client_id"], "oaiapp_1", "the registration is kept for the next sign-in");
    assert!(!login.account().await.expect("account").plan_usage);
    let _ = std::fs::remove_dir_all(&store);
}

#[tokio::test]
async fn signing_out_removes_the_record_and_nothing_else() {
    let store = dir("sign-out");
    std::fs::create_dir_all(&store).expect("dir");
    let record = json!({"issuer": "https://auth.test", "subject": "user-1", "client_id": "oaiapp_1",
        "ext_agent_host_id": "urn:uuid:x", "access_token": "live", "refresh_token": "r0",
        "scopes": ["chatgpt.tokens.use.direct"], "saved_at": 1});
    std::fs::write(store.join("siwc.json"), record.to_string()).expect("write");
    let kept = ["host-id", "models_cache.json", "notes.txt"];
    for name in kept {
        std::fs::write(store.join(name), name).expect("write");
    }
    // Nothing here renews; the token address is one nobody answers.
    let nowhere = SignInConfig { token_url: "http://127.0.0.1:1/token".into(), ..SignInConfig::default() };
    let login = ChatGptLogin::with_config(&store, nowhere.clone());
    assert_eq!(login.credential().await.expect("signed in"), Credential::Bearer("live".into()));

    assert!(login.sign_out().await.expect("sign out"));
    assert!(!ChatGptLogin::exists(&store));
    assert!(login.account().await.is_none());
    let err = login.credential().await.expect_err("signed out");
    assert!(matches!(&err, ModelError::Auth(m) if m.contains("not signed in")), "{err}");
    let mut left: Vec<String> = std::fs::read_dir(&store)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("name"))
        .collect();
    left.sort();
    assert_eq!(left, kept, "the other files stay, and the lock is released");
    assert!(!login.sign_out().await.expect("again"));

    // A store that was never signed in is not even created.
    let empty = dir("sign-out-empty");
    assert!(!ChatGptLogin::with_config(&empty, nowhere).sign_out().await.expect("nothing to do"));
    assert!(!empty.exists());
    let _ = std::fs::remove_dir_all(&store);
}
