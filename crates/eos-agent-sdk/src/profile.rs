//! What an agent gets for its model: the provider it runs on, its system
//! prompt, its tools and their wording, the request extras. Claude models
//! get Claude Code's (two wordings); OpenAI models get a Codex-style prompt
//! and the same tools, sent in full.
//!
//! A profile is built once per process and shared by every agent on it.
//! Per-model tool sets (Codex's own tools for GPT models) would be further
//! profiles chosen by [`Services::profile`](crate::services::Services::profile).

use std::sync::Arc;

use bytes::Bytes;
use eos_agent_core::{CacheTtl, ProviderId, SystemPrompt, Tool};

/// The kind of API a model runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    /// A ChatGPT plan through Sign in with ChatGPT.
    OpenAi,
    /// A provider the host configured ([`ProviderConfig`](crate::ProviderConfig)):
    /// its models are named `<provider>/<model>`.
    Other,
}

impl Provider {
    /// The provider of a model id or alias.
    pub fn of(model: &str) -> Self {
        if eos_agent_openai::is_openai_model(model) {
            return Self::OpenAi;
        }
        match model.split_once('/') {
            Some((provider, _)) if provider != eos_agent_anthropic::PROVIDER => Self::Other,
            _ => Self::Anthropic,
        }
    }

    /// The name of the provider `model` runs on, as credentials and recorded
    /// replies carry it.
    pub fn id_of(model: &str) -> ProviderId {
        ProviderId::new(match Self::of(model) {
            Self::Anthropic => eos_agent_anthropic::PROVIDER,
            Self::OpenAi => eos_agent_openai::PROVIDER,
            Self::Other => model.split_once('/').map_or(model, |(provider, _)| provider),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Other => "a provider",
        }
    }
}

pub(crate) struct ModelProfile {
    pub(crate) provider: Provider,
    /// The default system prompt of a top-level agent.
    pub(crate) system: SystemPrompt,
    /// The texts `system` is made of, for a host's appended prompt.
    pub(crate) system_texts: Vec<Arc<str>>,
    /// What a subagent's own prompt opens with.
    pub(crate) identity: Arc<str>,
    /// What a subagent's own prompt closes with (tool notes), if anything.
    pub(crate) subagent_suffix: Option<Arc<str>>,
    /// The process-wide tools as this profile words them (one list, whose
    /// address tells toolboxes of this wording from another's).
    pub(crate) base_tools: Arc<[Arc<dyn Tool>]>,
    /// Extra request members (Claude Code's `context_management`); `None` for OpenAI.
    pub(crate) request_extra: Option<Bytes>,
    /// Cache markers on system blocks and the newest message (`None`:
    /// the provider caches prefixes on its own).
    pub(crate) cache_ttl: Option<CacheTtl>,
    /// Rarely used tools wait behind ToolSearch (`false`: everything is
    /// sent in full and there is no ToolSearch).
    pub(crate) defer_tools: bool,
    /// Claude Code's full-prompt tool texts, also for the project's tools
    /// (the Agent tool).
    pub(crate) full_wording: bool,
}

impl ModelProfile {
    /// A system prompt of `texts`, its first and last blocks marked as this
    /// profile caches (Claude Code's breakpoints).
    pub(crate) fn prompt(&self, texts: &[&str]) -> SystemPrompt {
        let last = texts.len().saturating_sub(1);
        let blocks: Vec<(&str, Option<CacheTtl>)> =
            texts.iter().enumerate().map(|(i, t)| (*t, self.cache_ttl.filter(|_| i == 0 || i == last))).collect();
        SystemPrompt::new(&blocks)
    }
}
