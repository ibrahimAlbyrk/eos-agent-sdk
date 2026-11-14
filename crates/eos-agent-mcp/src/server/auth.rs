//! A server waiting for a sign-in: its auth tools, the sign-in flow, and
//! `/mcp`'s reconnect, sign-out and status.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use super::{Catalog, Connection, NEEDS_AUTH_RECHECK, NeedsAuth, Server, lock};
use crate::hub::McpServerState;
use crate::oauth::{ActiveFlow, Rejection, auth_tools};
use crate::tool::tool_name;

impl Server {
    pub(crate) fn needs_auth(&self) -> bool {
        lock(&self.needs_auth).is_some()
    }

    /// Starts (or rejoins) the sign-in; once it succeeds the server connects
    /// again with its own tools. `None` for a server without OAuth.
    /// `step_up`: the user asked for it on the step-up question.
    pub(crate) fn start_sign_in(&self, agent_id: Option<String>, step_up: bool) -> Option<(Arc<ActiveFlow>, bool)> {
        let auth = self.auth.as_ref()?;
        let (flow, reused) = auth.start_flow(agent_id, step_up);
        if !reused {
            let server = self.me.clone();
            let watched = Arc::clone(&flow);
            tokio::spawn(async move {
                if watched.wait().await.is_ok()
                    && let Some(server) = server.upgrade()
                {
                    server.signed_in().await;
                }
            });
        }
        Some((flow, reused))
    }

    pub(crate) fn active_sign_in(&self) -> Option<Arc<ActiveFlow>> {
        self.auth.as_ref()?.active_flow()
    }

    /// After a sign-in: connect again so the server's own tools come back.
    pub(crate) async fn signed_in(&self) {
        let _busy = self.busy();
        let mut slot = self.conn.lock().await;
        if !self.needs_auth() && slot.as_ref().and_then(Connection::live_peer).is_some() {
            return;
        }
        *lock(&self.needs_auth) = None;
        if let Some(old) = slot.take() {
            tokio::spawn(old.close());
        }
        match self.connect().await {
            Ok(conn) => *slot = Some(conn),
            Err(_) => {
                self.check_rejection();
            }
        }
    }

    /// Claude Code's `/mcp` "Reconnect": a fresh connection (and tool list).
    pub(crate) async fn reconnect(&self) -> Result<(), String> {
        let _busy = self.busy();
        let mut slot = self.conn.lock().await;
        if let Some(old) = slot.take() {
            tokio::spawn(old.close());
        }
        *lock(&self.needs_auth) = None;
        match self.connect().await {
            Ok(conn) => {
                *slot = Some(conn);
                Ok(())
            }
            Err(_) if self.check_rejection() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Claude Code's `/mcp` "Clear authentication": the stored sign-in is
    /// revoked and forgotten, then the server is connected again (and asks
    /// for a new sign-in). `false` for a server without OAuth.
    pub(crate) async fn sign_out(&self) -> Result<bool, String> {
        let Some(auth) = &self.auth else { return Ok(false) };
        auth.sign_out().await?;
        self.reconnect().await?;
        Ok(true)
    }

    /// Where the server stands now.
    pub(crate) fn state(&self) -> McpServerState {
        if self.needs_auth() {
            return McpServerState::NeedsAuth;
        }
        match self.conn.try_lock() {
            Ok(slot) if slot.as_ref().and_then(Connection::live_peer).is_some() => McpServerState::Connected,
            Ok(_) => McpServerState::Idle,
            // Someone is (re)connecting or calling it right now.
            Err(_) => McpServerState::Connected,
        }
    }

    /// What the agent is told while the server waits for a sign-in.
    pub(super) fn needs_auth_message(&self) -> String {
        let name = &self.config.name;
        let start = tool_name(name, "authenticate");
        match lock(&self.needs_auth).as_ref().map(|n| &n.rejection) {
            Some(Rejection::StepUp { scope, .. }) => format!(
                "MCP server \"{name}\" needs additional permissions (scope: \"{scope}\") — call `{start}` to \
                 re-authenticate"
            ),
            Some(Rejection::SignIn { token: Some(_) }) => {
                format!("MCP server \"{name}\" needs you to sign in again (call `{start}` to re-authenticate)")
            }
            _ => format!("MCP server \"{name}\" needs authentication (call `{start}` to sign in)"),
        }
    }

    /// When the server turned a request away for want of a sign-in, its
    /// tools become the auth tools until one is made.
    pub(super) fn check_rejection(&self) -> bool {
        let Some(rejection) = self.auth.as_ref().and_then(|a| a.take_rejection()) else { return false };
        if let Some(auth) = &self.auth {
            auth.note_needs_auth();
        }
        self.needs_sign_in(rejection);
        true
    }

    /// The server's tools become the auth tools until a sign-in is made.
    pub(super) fn needs_sign_in(&self, rejection: Rejection) {
        *lock(&self.needs_auth) = Some(NeedsAuth { rejection, checked: Instant::now() });
        let tools = self.auth_tools.get_or_init(|| auth_tools(self, &self.me)).clone();
        let mut slot = self.catalog.write().unwrap_or_else(PoisonError::into_inner);
        let same = slot.tools.len() == tools.len() && slot.tools.iter().zip(&tools).all(|(a, b)| Arc::ptr_eq(a, b));
        *slot = Arc::new(Catalog { tools, instructions: None, resources: false });
        if !same {
            self.version.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// While waiting for a sign-in: the message, unless a sign-in was made
    /// elsewhere since (then it is worth connecting again).
    pub(super) async fn still_needs_auth(&self) -> Option<String> {
        let rejected = {
            let mut needs = lock(&self.needs_auth);
            let n = needs.as_mut()?;
            if n.checked.elapsed() < NEEDS_AUTH_RECHECK {
                drop(needs);
                return Some(self.needs_auth_message());
            }
            n.checked = Instant::now();
            n.rejection.token().map(str::to_owned)
        };
        let auth = self.auth.as_ref()?;
        if auth.has_new_sign_in(rejected.as_deref()).await { None } else { Some(self.needs_auth_message()) }
    }
}
