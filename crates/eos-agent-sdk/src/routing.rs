//! One [`ModelClient`] over every configured provider, choosing by the
//! request's model, so the engine and the side tools keep a single client.

use std::sync::Arc;

use eos_agent_core::{BoxFuture, EventStream, ModelClient, ModelError, ModelRequest};

use crate::profile::Provider;

pub(crate) struct RoutingClient {
    pub(crate) anthropic: Option<Arc<dyn ModelClient>>,
    pub(crate) openai: Option<Arc<dyn ModelClient>>,
}

impl RoutingClient {
    pub(crate) fn has(&self, provider: Provider) -> bool {
        match provider {
            Provider::Anthropic => self.anthropic.is_some(),
            Provider::OpenAi => self.openai.is_some(),
        }
    }
}

impl ModelClient for RoutingClient {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        let provider = Provider::of(&request.model);
        let client = match provider {
            Provider::Anthropic => self.anthropic.as_ref(),
            Provider::OpenAi => self.openai.as_ref(),
        };
        match client {
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
