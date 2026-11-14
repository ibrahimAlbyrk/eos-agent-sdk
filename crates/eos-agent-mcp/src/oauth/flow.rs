//! The interactive sign-in (Claude Code's `performMCPOAuthFlow`): a callback
//! listener on 127.0.0.1, the authorization URL (PKCE S256, `state`,
//! `resource`), the user's browser, the code exchanged for tokens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Url;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::callback::{Callback, check_issuer, pick_port, registered_port, serve_callbacks};
use super::provider::{EndpointError, SavedBy, ServerAuth, lock};
use super::store::Entry;
use super::{AuthRequest, random_token};

/// Claude Code waits this long for the browser to come back.
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);
/// An `authenticate` call this soon after another reuses its sign-in.
const REUSE_WITHIN: Duration = Duration::from_secs(180);

/// A sign-in in progress.
pub(crate) struct ActiveFlow {
    state: String,
    started: Instant,
    url: watch::Receiver<Option<String>>,
    done: watch::Receiver<Option<Result<(), String>>>,
    submit: mpsc::UnboundedSender<Callback>,
    task: tokio::task::AbortHandle,
}

impl std::fmt::Debug for ActiveFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveFlow").field("started", &self.started).finish_non_exhaustive()
    }
}

impl ActiveFlow {
    fn finished(&self) -> bool {
        self.done.borrow().is_some()
    }

    /// The authorization URL once there is one; `Ok(None)` when the flow
    /// ended without needing the user.
    pub(crate) async fn auth_url(&self) -> Result<Option<String>, String> {
        let (mut url, mut done) = (self.url.clone(), self.done.clone());
        loop {
            if let Some(u) = url.borrow().clone() {
                return Ok(Some(u));
            }
            if let Some(result) = done.borrow().clone() {
                return result.map(|()| None);
            }
            tokio::select! {
                // The flow ended without an address: its result follows on `done`.
                changed = url.changed() => if changed.is_err() { return self.wait().await.map(|()| None) },
                changed = done.changed() => if changed.is_err() { return Err("The sign-in stopped.".into()) },
            }
        }
    }

    pub(crate) async fn wait(&self) -> Result<(), String> {
        let mut done = self.done.clone();
        loop {
            if let Some(result) = done.borrow().clone() {
                return result;
            }
            if done.changed().await.is_err() {
                return Err("Authentication was cancelled".into());
            }
        }
    }

    /// A callback URL pasted by the user. False when it carries no code or
    /// error, or belongs to another sign-in (its `state` differs).
    pub(crate) fn submit(&self, callback_url: &str) -> bool {
        let Ok(url) = Url::parse(callback_url) else { return false };
        let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (code, error) = (get("code"), get("error"));
        if code.is_none() && error.is_none() || get("state").as_deref() != Some(self.state.as_str()) {
            return false;
        }
        let callback = match (code, error) {
            (_, Some(e)) => {
                Callback::Denied(format!("OAuth error: {e} - {}", get("error_description").unwrap_or_default()))
            }
            (Some(code), None) => Callback::Code(code, get("iss")),
            (None, None) => return false,
        };
        self.submit.send(callback).is_ok()
    }

    fn cancel(&self) {
        self.task.abort();
    }
}

impl ServerAuth {
    /// The sign-in in progress, or a new one. A recent unfinished one is
    /// reused (its link stays valid); `reused` tells which. `step_up`: the
    /// user asked for it on the step-up question.
    pub(crate) fn start_flow(self: &Arc<Self>, agent_id: Option<String>, step_up: bool) -> (Arc<ActiveFlow>, bool) {
        let mut slot = lock(&self.flow);
        if let Some(flow) = slot.as_ref()
            && !flow.finished()
            && flow.started.elapsed() < REUSE_WITHIN
        {
            return (Arc::clone(flow), true);
        }
        if let Some(old) = slot.take() {
            old.cancel();
        }
        let state = random_token().unwrap_or_default();
        let (url_tx, url_rx) = watch::channel(None);
        let (done_tx, done_rx) = watch::channel(None);
        let (submit, submits) = mpsc::unbounded_channel();
        let me = Arc::clone(self);
        let flow_state = state.clone();
        let task = tokio::spawn(async move {
            let result = if flow_state.is_empty() {
                Err("no randomness for the OAuth state".to_owned())
            } else {
                me.run_flow(flow_state, url_tx, submits, agent_id, step_up).await
            };
            let _ = done_tx.send(Some(result));
        });
        let flow = Arc::new(ActiveFlow {
            state,
            started: Instant::now(),
            url: url_rx,
            done: done_rx,
            submit,
            task: task.abort_handle(),
        });
        *slot = Some(Arc::clone(&flow));
        (flow, false)
    }

    /// The sign-in in progress, if any (for pasted callback URLs).
    pub(crate) fn active_flow(&self) -> Option<Arc<ActiveFlow>> {
        lock(&self.flow).clone().filter(|f| !f.finished())
    }

    async fn run_flow(
        self: Arc<Self>,
        state: String,
        url_tx: watch::Sender<Option<String>>,
        mut submits: mpsc::UnboundedReceiver<Callback>,
        agent_id: Option<String>,
        asked: bool,
    ) -> Result<(), String> {
        if Url::parse(&self.url).is_err() {
            return Err("This server's URL is missing or not a valid URL, so sign-in can't start. Fix the URL in its \
                        MCP config (or set the environment variable it uses) and try again."
                .into());
        }
        if self.oauth.xaa {
            if !self.ctx.xaa_enabled() {
                return Err(format!(
                    "XAA is not enabled (set CLAUDE_CODE_ENABLE_XAA=1). Remove 'oauth.xaa' from server '{}' to use \
                     the standard consent flow.",
                    self.name
                ));
            }
            let on_url = move |url: String| {
                let _ = url_tx.send(Some(url));
            };
            return self.xaa_sign_in(agent_id, &on_url).await;
        }
        let credentials = &self.ctx.credentials;
        let stored = credentials.entry(&self.key).await?;
        let configured_scope = self.oauth.scopes.is_some() || self.oauth.auth_server_metadata_url.is_some();
        let step_up = stored.step_up_scope.clone().filter(|_| !configured_scope);
        let advertised = stored.resource_metadata_url.clone().or_else(|| lock(&self.advertised).clone());

        let registered = registered_port(&stored);
        let port = match self.oauth.callback_port {
            Some(p) => p,
            None => pick_port(registered)?,
        };
        let redirect = format!("http://localhost:{port}/callback");
        self.clear_for_sign_in(&stored, registered == Some(port), &redirect).await;
        let stored = credentials.entry(&self.key).await?;

        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "OAuth callback port {port} is already in use — another process may be holding it. Run `lsof -ti:{port} \
                     -sTCP:LISTEN` to find it."
                )
            } else {
                format!("OAuth callback server failed: {e}")
            }
        })?;
        let (tx, mut callbacks) = mpsc::unbounded_channel();
        let listening = tokio::spawn(serve_callbacks(listener, state.clone(), redirect.clone(), tx));
        let _stop_listening = AbortOnDrop(listening.abort_handle());

        let plan = self.plan(&stored, &redirect, &state, step_up.as_deref(), advertised.as_deref()).await?;
        let _ = url_tx.send(Some(plan.url.clone()));
        self.ctx.prompter.authorize(&AuthRequest {
            server: self.name.clone(),
            url: plan.url.clone(),
            redirect_uri: redirect.clone(),
            agent_id,
            step_up: asked,
        });

        let callback = tokio::select! {
            Some(c) = callbacks.recv() => c,
            Some(c) = submits.recv() => c,
            () = tokio::time::sleep(FLOW_TIMEOUT) => return Err("Authentication timeout".into()),
        };
        let code = match callback {
            Callback::Code(code, iss) => {
                check_issuer(plan.metadata.as_ref(), iss.as_deref())?;
                code
            }
            Callback::Denied(why) => return Err(why),
        };
        let grant = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("code_verifier", plan.verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
        ];
        let tokens = self
            .token_request(&plan.auth_server, plan.metadata.as_ref(), &plan.client, &grant, plan.resource.as_deref())
            .await;
        let tokens = match tokens {
            Ok(t) => t,
            Err(EndpointError::OAuth { code, description })
                if code == "invalid_client" || code == "unauthorized_client" =>
            {
                let _ = credentials
                    .mutate_entry(&self.key, |e| {
                        e.remove("clientId");
                        e.remove("clientSecret");
                        true
                    })
                    .await;
                return Err(EndpointError::OAuth { code, description }.to_string());
            }
            Err(e) => return Err(format!("Token exchange failed: {e}")),
        };
        if let Some(metadata) = &plan.metadata {
            *lock(&self.metadata) = Some((plan.auth_server.clone(), metadata.clone()));
        }
        let by = SavedBy::SignIn {
            auth_server: &plan.auth_server,
            found: plan.metadata.is_some(),
            served: plan.served.as_deref(),
        };
        self.save_tokens(&tokens, by).await;
        Ok(())
    }

    /// Claude Code clears the old sign-in first, keeping a client
    /// registration made for this same redirect.
    async fn clear_for_sign_in(&self, stored: &Entry, same_port: bool, redirect: &str) {
        self.set_cached(None);
        if stored == &Entry::default() {
            return;
        }
        let keep_client = stored.client_id.is_some() && (same_port || stored.redirect_uri.as_deref() == Some(redirect));
        let _ = self
            .ctx
            .credentials
            .mutate(|root| {
                let Some(all) = root.get_mut("mcpOAuth").and_then(Value::as_object_mut) else { return false };
                if !keep_client {
                    return all.remove(&self.key).is_some();
                }
                let Some(e) = all.get_mut(&self.key).and_then(Value::as_object_mut) else { return false };
                if e.get("accessToken").and_then(Value::as_str).is_none_or(str::is_empty)
                    && !e.contains_key("refreshToken")
                {
                    return false;
                }
                e.insert("accessToken".into(), "".into());
                e.remove("refreshToken");
                e.remove("scope");
                e.insert("expiresAt".into(), 0.into());
                true
            })
            .await;
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
