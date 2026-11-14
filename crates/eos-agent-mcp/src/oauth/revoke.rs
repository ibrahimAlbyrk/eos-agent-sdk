//! Claude Code's "Clear authentication" in `/mcp`: the stored tokens are
//! revoked at the authorization server (RFC 7009: the refresh token, then
//! the access token) and the server's entry leaves the credentials store.

use serde_json::Value;

use super::provider::{ServerAuth, basic_auth, lock};
use super::store::Entry;

impl ServerAuth {
    /// Revokes the sign-in where the server allows it, then forgets it.
    pub(crate) async fn sign_out(&self) -> Result<(), String> {
        let entry = self.ctx.credentials.entry(&self.key).await?;
        let has_tokens = entry.access_token.as_deref().is_some_and(|t| !t.is_empty()) || entry.refresh_token.is_some();
        if has_tokens {
            self.revoke(&entry).await;
        }
        self.set_cached(None);
        lock(&self.metadata).take();
        self.ctx
            .credentials
            .mutate(|root| {
                root.get_mut("mcpOAuth")
                    .and_then(Value::as_object_mut)
                    .is_some_and(|all| all.remove(&self.key).is_some())
            })
            .await
    }

    async fn revoke(&self, entry: &Entry) {
        let Some((_, metadata)) = self.known_metadata(entry).await else { return };
        let Some(endpoint) = metadata.get("revocation_endpoint").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let methods: Option<Vec<&str>> = metadata
            .get("revocation_endpoint_auth_methods_supported")
            .or_else(|| metadata.get("token_endpoint_auth_methods_supported"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect());
        // Claude Code: basic unless the server takes only `client_secret_post`.
        let post = methods.is_some_and(|m| !m.contains(&"client_secret_basic") && m.contains(&"client_secret_post"));
        let client = self.stored_client(entry);
        let access = entry.access_token.clone().filter(|t| !t.is_empty());
        let tokens = [(entry.refresh_token.clone(), "refresh_token"), (access.clone(), "access_token")];
        for (token, hint) in tokens {
            let Some(token) = token else { continue };
            let mut form: Vec<(&str, &str)> = vec![("token", &token), ("token_type_hint", hint)];
            let mut basic = None;
            if let Some(c) = &client {
                match &c.secret {
                    Some(secret) if post => form.extend([("client_id", c.id.as_str()), ("client_secret", secret)]),
                    Some(secret) => basic = Some(format!("Basic {}", basic_auth(&c.id, secret))),
                    None => form.push(("client_id", c.id.as_str())),
                }
            }
            let answer = self.http.post_form_as(&endpoint, &form, basic.as_deref()).await;
            // A 401: once more with the access token as a bearer.
            if answer.as_ref().is_ok_and(|a| a.status == 401)
                && let Some(access) = &access
            {
                form.retain(|(k, _)| *k != "client_id" && *k != "client_secret");
                let _ = self.http.post_form_as(&endpoint, &form, Some(&format!("Bearer {access}"))).await;
            }
        }
    }
}
