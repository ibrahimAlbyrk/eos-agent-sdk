//! Credentials per agent: the account an agent (and its subagents) bills,
//! swapped while it lives, and the host's say when one stops working.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use eos_agent_anthropic::auth::ClaudeLogin;
use eos_agent_core::{BoxFuture, Credential, ModelError, RateLimitInfo, RequestAuth, StaticCredential, TokenSource};

use crate::profile::Provider;

/// How many credentials in a row may turn out unusable before one is read
/// (a host could hand out signed-out ones forever).
const MAX_REPLACEMENTS: u32 = 8;

/// One credential a host hands to agents: a Claude sign-in store, an API
/// key, a ChatGPT sign-in. It remembers the plan-limit state its latest
/// response carried, for a usage display. Share one `Account` between all
/// agents on the same sign-in: renewals then happen once, in turn.
pub struct Account {
    source: Arc<dyn TokenSource>,
    limits: Mutex<Option<RateLimitInfo>>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("limits", &self.rate_limit()).finish_non_exhaustive()
    }
}

impl Account {
    pub fn new(source: Arc<dyn TokenSource>) -> Self {
        Self { source, limits: Mutex::new(None) }
    }

    /// The Claude sign-in kept for `store_dir` (Claude Code's
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR`), renewed under Claude Code's lock.
    pub fn claude_store(store_dir: &Path) -> Self {
        Self::new(Arc::new(ClaudeLogin::keychain(Some(store_dir))))
    }

    pub fn api_key(key: impl Into<String>) -> Self {
        Self::new(Arc::new(StaticCredential(Credential::ApiKey(key.into()))))
    }

    /// The plan-limit state of the latest response on this account that
    /// reported one (`status` is `rejected` while its limit is reached).
    pub fn rate_limit(&self) -> Option<RateLimitInfo> {
        self.limits.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl TokenSource for Account {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        self.source.credential()
    }

    fn refused<'a>(&'a self, rejected: &'a Credential) -> BoxFuture<'a, bool> {
        self.source.refused(rejected)
    }

    fn failed<'a>(&'a self, rejected: &'a Credential, error: &'a ModelError) -> BoxFuture<'a, bool> {
        self.source.failed(rejected, error)
    }

    fn observe(&self, limits: &RateLimitInfo) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner) = Some(limits.clone());
        self.source.observe(limits);
    }
}

/// A credential source as an agent option (opaque in debug output).
#[derive(Clone)]
pub struct AuthSource(pub Arc<dyn TokenSource>);

impl std::fmt::Debug for AuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthSource(..)")
    }
}

/// Why an agent's credential stopped working.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthFailure {
    /// Its plan's limit is reached; `resets_at` says until when.
    LimitReached(RateLimitInfo),
    /// Its sign-in is over (expired and not renewable, or revoked).
    Expired(String),
}

/// The host's say when an agent's credential stops working. It is asked
/// while the failed request waits, before the turn fails: with another
/// credential the request goes out again and the turn carries on; with
/// `None` the turn ends in [`ModelError::LimitReached`] or
/// [`ModelError::Auth`], and the host may later call
/// [`SdkAgent::set_auth`](crate::SdkAgent::set_auth) and
/// [`SdkAgent::resume`](crate::SdkAgent::resume).
pub trait AuthFailover: Send + Sync {
    /// `agent_id` is the top-level agent (its subagents share its credential).
    fn replacement<'a>(
        &'a self,
        agent_id: &'a str,
        failure: &'a AuthFailure,
    ) -> BoxFuture<'a, Option<Arc<dyn TokenSource>>>;
}

/// The credential of one top-level agent and its subagents: read before
/// every request, replaceable at any time.
pub(crate) struct SessionAuth {
    agent_id: Arc<str>,
    current: RwLock<Arc<dyn TokenSource>>,
    failover: Option<Arc<dyn AuthFailover>>,
    /// One replacement at a time: agents of a tree that fail together ask the host once.
    replacing: tokio::sync::Mutex<()>,
}

impl SessionAuth {
    pub(crate) fn new(
        agent_id: &str,
        source: Arc<dyn TokenSource>,
        failover: Option<Arc<dyn AuthFailover>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_id: agent_id.into(),
            current: RwLock::new(source),
            failover,
            replacing: tokio::sync::Mutex::default(),
        })
    }

    /// As a request's credential for `provider`.
    pub(crate) fn for_requests(self: &Arc<Self>, provider: Provider) -> RequestAuth {
        RequestAuth { provider: provider.id(), source: Arc::clone(self) as Arc<dyn TokenSource> }
    }

    pub(crate) fn set(&self, source: Arc<dyn TokenSource>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = source;
    }

    fn current(&self) -> Arc<dyn TokenSource> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Asks the host for another credential in place of `failed`, unless
    /// one was put in its place meanwhile. True when the credential changed.
    async fn replace(&self, failed: &Arc<dyn TokenSource>, failure: AuthFailure) -> bool {
        let _one = self.replacing.lock().await;
        if !Arc::ptr_eq(&self.current(), failed) {
            return true;
        }
        let Some(failover) = &self.failover else { return false };
        match failover.replacement(&self.agent_id, &failure).await {
            Some(source) => {
                self.set(source);
                true
            }
            None => false,
        }
    }
}

impl TokenSource for SessionAuth {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        Box::pin(async move {
            let mut replaced = 0;
            loop {
                let source = self.current();
                match source.credential().await {
                    Err(ModelError::Auth(why)) if replaced < MAX_REPLACEMENTS => {
                        if !self.replace(&source, AuthFailure::Expired(why.clone())).await {
                            return Err(ModelError::Auth(why));
                        }
                        replaced += 1;
                    }
                    other => return other,
                }
            }
        })
    }

    fn refused<'a>(&'a self, rejected: &'a Credential) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.current().refused(rejected).await })
    }

    fn failed<'a>(&'a self, rejected: &'a Credential, error: &'a ModelError) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let failure = match error {
                ModelError::LimitReached { info } => AuthFailure::LimitReached(info.clone()),
                ModelError::Auth(why) => AuthFailure::Expired(why.clone()),
                _ => return false,
            };
            let source = self.current();
            // Another agent of the tree failed first and the credential was
            // replaced since `rejected` was read: use the new one.
            if source.credential().await.is_ok_and(|now| now != *rejected) {
                return true;
            }
            self.replace(&source, failure).await
        })
    }

    fn observe(&self, limits: &RateLimitInfo) {
        self.current().observe(limits);
    }
}
