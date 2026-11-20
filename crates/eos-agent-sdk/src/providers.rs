//! The providers a host adds by configuration: servers that speak the Chat
//! Completions API, or OpenAI's Responses API with an API key. A model of
//! one is named `<provider>/<model>`. They can be added, replaced and
//! removed while agents run.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use eos_agent_core::{ModelClient, ModelError, ProviderId, TokenSource};
use eos_agent_openai::chat::{ChatClient, ChatConfig, Compat, Effort};
use eos_agent_openai::{ClientConfig, OpenAiClient, ToolPlacement};

/// The context window of a model whose provider's configuration gives none.
const CONTEXT_WINDOW: u64 = 128_000;
const MAX_TOKENS: u32 = 8_192;

/// The API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wire {
    /// `POST <base_url><chat_path>`.
    #[default]
    Chat,
    /// OpenAI's Responses API: `POST <base_url>/responses`, plain function
    /// tools; of the [`Compat`], only `efforts` holds.
    Responses,
}

/// One provider as a host configures it.
#[derive(Clone)]
pub struct ProviderConfig {
    /// Its name here: what its models' ids open with (`deepseek` in `deepseek/deepseek-chat`).
    pub id: String,
    pub wire: Wire,
    /// The API root (`https://api.deepseek.com/v1`).
    pub base_url: String,
    /// Where chat completions are under the root (`None`: `/chat/completions`).
    pub chat_path: Option<String>,
    /// The key (an empty one for a server that asks for none).
    pub auth: Arc<dyn TokenSource>,
    /// The header the key goes in (`None`: `Authorization: Bearer`).
    pub key_header: Option<String>,
    /// Sent with every request.
    pub headers: Vec<(String, String)>,
    /// The header an agent's id goes in with its chat requests, for a server
    /// that routes by conversation (`None`: none is sent).
    pub session_header: Option<String>,
    pub compat: Compat,
    /// Its models' context window and longest reply, in tokens (`None`: a modest default).
    pub context_window: Option<u64>,
    pub max_tokens: Option<u32>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig").field("id", &self.id).field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

enum Client {
    Chat(Arc<ChatClient>),
    Responses(Arc<OpenAiClient>),
}

pub(crate) struct Registered {
    client: Client,
    pub(crate) auth: Arc<dyn TokenSource>,
    pub(crate) context_window: u64,
    pub(crate) max_tokens: u32,
    compat: Compat,
}

impl Registered {
    fn model_client(&self) -> Arc<dyn ModelClient> {
        match &self.client {
            Client::Chat(client) => Arc::clone(client) as Arc<dyn ModelClient>,
            Client::Responses(client) => Arc::clone(client) as Arc<dyn ModelClient>,
        }
    }

    /// The ids of the models the provider lists.
    pub(crate) async fn list_models(&self) -> Result<Vec<String>, ModelError> {
        match &self.client {
            Client::Chat(client) => client.list_models().await,
            Client::Responses(client) => {
                let doc = client.list_models().await?;
                let list = doc["data"].as_array().cloned().unwrap_or_default();
                Ok(list.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect())
            }
        }
    }

    /// How `model` (as the provider names it) takes an effort.
    pub(crate) fn effort(&self, model: &str) -> &Effort {
        self.compat.effort(model)
    }
}

#[derive(Default)]
pub(crate) struct Providers(RwLock<HashMap<ProviderId, Arc<Registered>>>);

impl Providers {
    pub(crate) fn get(&self, id: &ProviderId) -> Option<Arc<Registered>> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).get(id).cloned()
    }

    /// The providers' names, in order.
    pub(crate) fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> =
            self.0.read().unwrap_or_else(PoisonError::into_inner).keys().map(|id| id.as_str().to_owned()).collect();
        ids.sort();
        ids
    }

    pub(crate) fn client(&self, id: &ProviderId) -> Option<Arc<dyn ModelClient>> {
        self.get(id).map(|p| p.model_client())
    }

    pub(crate) fn set(&self, config: ProviderConfig) -> Result<(), ModelError> {
        let id = ProviderId::new(&config.id);
        let client = match config.wire {
            Wire::Chat => {
                let mut chat = ChatConfig::new(config.id, config.base_url);
                chat.chat_path = config.chat_path.unwrap_or(chat.chat_path);
                chat.key_header = config.key_header;
                chat.headers = config.headers;
                chat.session_header = config.session_header;
                chat.compat = config.compat.clone();
                Client::Chat(Arc::new(ChatClient::new(Arc::clone(&config.auth), chat)?))
            }
            Wire::Responses => {
                let responses = ClientConfig {
                    provider: config.id,
                    base_url: config.base_url.trim_end_matches('/').to_owned(),
                    tools: ToolPlacement::TopLevel,
                    ..ClientConfig::default()
                };
                Client::Responses(Arc::new(OpenAiClient::new(Arc::clone(&config.auth), responses)?))
            }
        };
        let registered = Registered {
            client,
            compat: config.compat,
            auth: config.auth,
            context_window: config.context_window.unwrap_or(CONTEXT_WINDOW),
            max_tokens: config.max_tokens.unwrap_or(MAX_TOKENS),
        };
        self.0.write().unwrap_or_else(PoisonError::into_inner).insert(id, Arc::new(registered));
        Ok(())
    }

    pub(crate) fn remove(&self, id: &ProviderId) -> bool {
        self.0.write().unwrap_or_else(PoisonError::into_inner).remove(id).is_some()
    }
}
