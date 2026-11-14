//! Claude Code's in-call step-up: a tool call the server refused for want of
//! a wider scope asks the user (through the host's prompter) whether to sign
//! in again; after the sign-in the server is connected again and the call
//! retried once, its result led by a line naming the new scope. A scope that
//! is refused again right after its own re-authentication is not asked about
//! again in this process.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Value, json};

use super::{Server, lock};
use crate::config::McpTransport;
use crate::oauth::{STEP_UP_CHOICES, StepUpAnswer, StepUpRequest};

/// How a step-up went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Reauthenticated,
    Declined,
    Failed,
}

pub(super) type Flight = Shared<BoxFuture<'static, Outcome>>;

/// Claude Code's `stepUpReauthExhausted`, by server key: scopes refused again
/// after the user re-authenticated for them.
fn exhausted() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static MAP: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    MAP.get_or_init(Mutex::default)
}

/// Scopes as a set, written one way (`Vo`).
fn normalized(scope: &str) -> String {
    let mut parts: Vec<&str> = scope.split(' ').filter(|s| !s.is_empty()).collect();
    parts.sort_unstable();
    parts.join(" ")
}

/// Whether every scope of `wanted` is among `configured` (`Fa`).
fn within(configured: &str, wanted: &str) -> bool {
    let have: HashSet<&str> = configured.split(' ').filter(|s| !s.is_empty()).collect();
    wanted.split(' ').filter(|s| !s.is_empty()).all(|s| have.contains(s))
}

/// Claude Code shows the scope only when its display sanitizer leaves it
/// unchanged (printable, no quotes or angle brackets, at most 200 characters).
fn displayable(scope: &str) -> bool {
    scope.chars().count() <= 200
        && scope.chars().all(|c| {
            !c.is_control()
                && !matches!(c, '<' | '>' | '"' | ';' | '‘' | '’' | '‚' | '“' | '”' | '„' | '«' | '»')
                && !('\u{200b}'..='\u{200f}').contains(&c)
                && !('\u{202a}'..='\u{202e}').contains(&c)
                && !('\u{2066}'..='\u{2069}').contains(&c)
                && c != '\u{feff}'
        })
}

/// Claude Code's `Ps`: the retried result is led by the re-authentication.
pub(super) fn lead_with_reauth(result: &mut Value, server: &str, scope: &str) {
    let line = format!("Re-authenticated MCP server \"{server}\" (scope: \"{scope}\")");
    match result.get_mut("content") {
        Some(Value::Array(blocks)) => blocks.insert(0, json!({"type": "text", "text": line})),
        _ => result["content"] = json!([{"type": "text", "text": line}]),
    }
}

impl Server {
    /// Claude Code asks only for Streamable HTTP servers without XAA, for a
    /// scope inside the configured ones (any, when the config names neither
    /// scopes nor a metadata URL), not refused again after a re-authentication.
    pub(super) fn may_ask_step_up(&self, scope: &str) -> bool {
        let (McpTransport::Http { oauth, .. }, Some(auth)) = (&self.config.transport, &self.auth) else {
            return false;
        };
        let oauth = oauth.clone().unwrap_or_default();
        if oauth.xaa {
            return false;
        }
        let in_config = match (&oauth.scopes, &oauth.auth_server_metadata_url) {
            (None, None) => true,
            (Some(configured), _) => within(configured, scope),
            (None, Some(_)) => false,
        };
        in_config && !lock(exhausted()).get(&auth.key).is_some_and(|s| s.contains(&normalized(scope)))
    }

    /// The retried call was refused again: a scope refused right after its
    /// own re-authentication is not asked about again (`Ms`).
    pub(super) fn note_rechallenged(&self, reauthed: &str, requested: &str) {
        let Some(auth) = &self.auth else { return };
        if normalized(reauthed) == normalized(requested) {
            lock(exhausted()).entry(auth.key.clone()).or_default().insert(normalized(requested));
        }
    }

    /// Asks, signs in and reconnects; callers for the same server share one
    /// question (Claude Code's `stepUpReauthFlights`).
    pub(super) async fn step_up(&self, scope: &str, agent_id: &str) -> Outcome {
        let flight = {
            let mut slot = lock(&self.step_up_flight);
            if let Some(flight) = slot.as_ref().filter(|f| f.peek().is_none()) {
                flight.clone()
            } else {
                let Some(me) = self.me.upgrade() else { return Outcome::Failed };
                let (scope, agent) = (scope.to_owned(), agent_id.to_owned());
                let flight = async move { me.run_step_up(&scope, &agent).await }.boxed().shared();
                *slot = Some(flight.clone());
                flight
            }
        };
        flight.await
    }

    async fn run_step_up(&self, scope: &str, agent_id: &str) -> Outcome {
        let Some(auth) = &self.auth else { return Outcome::Failed };
        if !displayable(scope) {
            return Outcome::Declined;
        }
        let name = &self.config.name;
        let request = StepUpRequest {
            server: name.clone(),
            scope: scope.to_owned(),
            question: format!(
                "MCP server \"{name}\" needs additional permissions for this tool (it asked for the scope \
                 \"{scope}\"). Re-authenticate now?"
            ),
            choices: STEP_UP_CHOICES.to_vec(),
            agent_id: Some(agent_id.to_owned()),
        };
        if auth.ask_step_up(&request).await != StepUpAnswer::Reauthenticate {
            return Outcome::Declined;
        }
        let Some((flow, _)) = self.start_sign_in(Some(agent_id.to_owned()), true) else { return Outcome::Failed };
        if flow.wait().await.is_err() {
            return Outcome::Failed;
        }
        match self.reconnect().await {
            Ok(()) if !self.needs_auth() => Outcome::Reauthenticated,
            _ => Outcome::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_compare_as_sets() {
        assert_eq!(normalized("b  a c"), "a b c");
        assert!(within("read write admin", "admin read"));
        assert!(!within("read", "admin"));
        assert!(displayable("repo:write admin"));
        assert!(!displayable("a\"b"));
        assert!(!displayable(&"x".repeat(201)));
    }

    #[test]
    fn the_retried_result_names_the_new_scope() {
        let mut result = json!({"content": [{"type": "text", "text": "done"}]});
        lead_with_reauth(&mut result, "docs", "admin");
        assert_eq!(result["content"][0]["text"], "Re-authenticated MCP server \"docs\" (scope: \"admin\")");
        assert_eq!(result["content"][1]["text"], "done");
    }
}
