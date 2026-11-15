//! One [`ModelClient`] over every configured provider, choosing by the
//! request's model, so the engine and the side tools keep a single client.

use std::collections::HashMap;
use std::sync::Arc;

use eos_agent_core::{BoxFuture, EventStream, ModelClient, ModelError, ModelRequest, ProviderId};

use crate::profile::Provider;

#[derive(Default)]
pub(crate) struct RoutingClient {
    clients: HashMap<ProviderId, Arc<dyn ModelClient>>,
}

impl RoutingClient {
    pub(crate) fn add(&mut self, provider: ProviderId, client: Arc<dyn ModelClient>) {
        self.clients.insert(provider, client);
    }

    pub(crate) fn has(&self, provider: &ProviderId) -> bool {
        self.clients.contains_key(provider)
    }
}

impl ModelClient for RoutingClient {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        let provider = Provider::of(&request.model);
        match self.clients.get(&provider.id()) {
            Some(client) => client.stream(request),
            None => {
                let how = match provider {
                    Provider::Anthropic => "pass a Claude credential to the SDK",
                    Provider::OpenAi => "sign in with `eos-agent login chatgpt` and pass the store to the SDK",
                };
                let message = format!("{} has no {} credential here; {how}", request.model, provider.name());
                Box::pin(async move { Err(ModelError::Auth(message)) })
            }
        }
    }
}
