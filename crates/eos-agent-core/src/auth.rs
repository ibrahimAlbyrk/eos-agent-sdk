use std::sync::Arc;

use crate::BoxFuture;
use crate::model::{ModelError, RateLimitInfo};

/// A credential for Anthropic's APIs.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    /// A Claude subscription OAuth access token.
    Bearer(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ApiKey(_) => "ApiKey(<redacted>)",
            Self::Bearer(_) => "Bearer(<redacted>)",
        })
    }
}

/// Where a client gets its credential before each request.
pub trait TokenSource: Send + Sync {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>>;

    /// The server refused `rejected`. Returns true when a fresh credential is
    /// now available (worth one retry).
    fn refused<'a>(&'a self, _rejected: &'a Credential) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// `rejected` cannot be used for now: its plan's limit is reached
    /// ([`ModelError::LimitReached`]) or its sign-in is over
    /// ([`ModelError::Auth`], after [`refused`](Self::refused) could not
    /// renew it). Returns true when this source now gives a different
    /// credential (worth sending the request again).
    fn failed<'a>(&'a self, _rejected: &'a Credential, _error: &'a ModelError) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// The plan-limit state the server reported with a response to this
    /// source's credential.
    fn observe(&self, _limits: &RateLimitInfo) {}
}

/// The name of a configured provider (`anthropic`, `openai`, a user's own
/// endpoint): what routes a request to its client and labels a credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(Arc<str>);

impl ProviderId {
    pub fn new(id: &str) -> Self {
        Self(Arc::from(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The credential a request (and the agent sending it) uses instead of the
/// client's own. A client ignores one made for another provider.
#[derive(Clone)]
pub struct RequestAuth {
    /// The provider the credential is for, as its client names itself.
    pub provider: ProviderId,
    pub source: Arc<dyn TokenSource>,
}

impl std::fmt::Debug for RequestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestAuth").field("provider", &self.provider).finish_non_exhaustive()
    }
}

impl RequestAuth {
    /// The source to use for a request to `provider`: this one when it is
    /// for that provider, else the client's `own`.
    pub fn choose<'a>(request: Option<&'a Self>, provider: &str, own: &'a Arc<dyn TokenSource>) -> &'a dyn TokenSource {
        match request {
            Some(auth) if auth.provider.as_str() == provider => &*auth.source,
            _ => &**own,
        }
    }
}

/// Where a secret document lives (a Keychain item, a 0600 file): Claude
/// Code's credentials JSON, which holds the Claude sign-in and MCP sign-ins.
pub trait SecretStore: Send + Sync {
    fn read(&self) -> BoxFuture<'_, std::io::Result<Option<String>>>;
    fn write<'a>(&'a self, value: &'a str) -> BoxFuture<'a, std::io::Result<()>>;
}

/// A credential that never changes.
#[derive(Debug, Clone)]
pub struct StaticCredential(pub Credential);

impl TokenSource for StaticCredential {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        let c = self.0.clone();
        Box::pin(async move { Ok(c) })
    }
}
