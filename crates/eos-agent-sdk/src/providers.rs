//! The providers a host adds by configuration: servers that speak the Chat
//! Completions API. A model of one is named `<provider>/<model>`. They can
//! be added, replaced and removed while agents run.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use eos_agent_core::{ModelClient, ModelError, ProviderId, TokenSource};
use eos_agent_openai::chat::{ChatClient, ChatConfig, Compat};

/// The context window of a model whose provider's configuration gives none.
const CONTEXT_WINDOW: u64 = 128_000;
const MAX_TOKENS: u32 = 8_192;

/// One provider as a host configures it.
#[derive(Clone)]
pub struct ProviderConfig {
    /// Its name here: what its models' ids open with (`deepseek` in `deepseek/deepseek-chat`).
    pub id: String,
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

pub(crate) struct Registered {
    pub(crate) client: Arc<ChatClient>,
    pub(crate) auth: Arc<dyn TokenSource>,
    pub(crate) context_window: u64,
    pub(crate) max_tokens: u32,
    /// Requests carry an effort.
    pub(crate) effort: bool,
}

#[derive(Default)]
pub(crate) struct Providers(RwLock<HashMap<ProviderId, Arc<Registered>>>);

impl Providers {
    pub(crate) fn get(&self, id: &ProviderId) -> Option<Arc<Registered>> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).get(id).cloned()
    }

    pub(crate) fn client(&self, id: &ProviderId) -> Option<Arc<dyn ModelClient>> {
        self.get(id).map(|p| Arc::clone(&p.client) as Arc<dyn ModelClient>)
    }

    pub(crate) fn set(&self, config: ProviderConfig) -> Result<(), ModelError> {
        let id = ProviderId::new(&config.id);
        let mut chat = ChatConfig::new(config.id, config.base_url);
        chat.chat_path = config.chat_path.unwrap_or(chat.chat_path);
        chat.key_header = config.key_header;
        chat.headers = config.headers;
        chat.compat = config.compat;
        let registered = Registered {
            effort: chat.compat.effort,
            client: Arc::new(ChatClient::new(Arc::clone(&config.auth), chat)?),
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
