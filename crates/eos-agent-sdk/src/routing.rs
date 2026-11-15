//! One [`ModelClient`] over every configured provider, choosing by the
//! request's model, so the engine and the side tools keep a single client.

use std::collections::HashMap;
use std::sync::Arc;

use eos_agent_core::{BoxFuture, EventStream, ModelClient, ModelError, ModelRequest, ProviderId};

use crate::profile::Provider;
use crate::providers::Providers;

pub(crate) struct RoutingClient {
    clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
    /// The providers the host configured, which come and go.
    others: Arc<Providers>,
}

impl RoutingClient {
    pub(crate) fn new(others: Arc<Providers>) -> Self {
        Self { clients: HashMap::new(), others }
    }

    pub(crate) fn add(&mut self, provider: ProviderId, client: Arc<dyn ModelClient>) {
        self.clients.insert(provider, client);
    }

    pub(crate) fn has(&self, provider: &ProviderId) -> bool {
        self.clients.contains_key(provider)
    }
}

impl ModelClient for RoutingClient {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        let id = Provider::id_of(&request.model);
        let client = self.clients.get(&id).cloned().or_else(|| self.others.client(&id));
        Box::pin(async move {
            match client {
                Some(client) => client.stream(request).await,
                None => {
                    let how = match Provider::of(&request.model) {
                        Provider::Anthropic => "pass a Claude credential to the SDK",
                        Provider::OpenAi => "sign in with `eos-agent login chatgpt` and pass the store to the SDK",
                        Provider::Other => "add the provider and its key",
                    };
                    Err(ModelError::Auth(format!("{} has no credential here; {how}", request.model)))
                }
            }
        })
    }
}
