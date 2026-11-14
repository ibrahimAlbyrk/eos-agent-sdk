//! Method parameters as the client sends them (camelCase, see `docs/protocol.md`).

use std::path::PathBuf;

use eos_agent_sdk::{AgentOptions, McpConfig, SandboxConfig, SystemPromptOverride};
use eos_agent_types::ContentBlock;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::rpc::RpcError;

pub(crate) fn parse<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    serde_json::from_value(params.unwrap_or_else(|| Value::Object(Default::default()))).map_err(RpcError::params)
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub(crate) struct Initialize {
    /// One credential, or several (one per provider).
    pub(crate) auth: Option<Auths>,
    pub(crate) base_url: Option<String>,
    /// Where OpenAI requests go (default `https://api.openai.com/v1`).
    pub(crate) openai_base_url: Option<String>,
    pub(crate) model: Option<String>,
    /// One JSONL transcript per agent here; needed for resume and eviction.
    pub(crate) transcripts: Option<PathBuf>,
    pub(crate) data_dir: Option<PathBuf>,
    /// Seconds idle before an agent's history leaves memory; 0 = never.
    pub(crate) idle_eviction_secs: Option<u64>,
    pub(crate) permissions: Permissions,
    pub(crate) permission_timeout_ms: Option<u64>,
    pub(crate) ask_timeout_ms: Option<u64>,
    pub(crate) artifacts: Option<bool>,
    /// The artifact tools' own credential (default: `auth`'s).
    pub(crate) artifacts_auth: Option<Auth>,
    /// Claude Code's `sandbox` settings, over the ones in its settings files.
    pub(crate) sandbox: Option<SandboxConfig>,
    /// The Workflow tool on or off (default: as Claude Code decides it).
    pub(crate) workflows: Option<bool>,
    /// Where workflow runs keep their files (default: `dataDir`).
    pub(crate) workflow_dir: Option<PathBuf>,
}

/// `auth`: one credential or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Auths {
    One(Auth),
    Many(Vec<Auth>),
}

impl Auths {
    pub(crate) fn all(&self) -> &[Auth] {
        match self {
            Self::One(auth) => std::slice::from_ref(auth),
            Self::Many(list) => list,
        }
    }
}

/// Where a credential comes from.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Auth {
    /// A Claude sign-in store directory (Claude Code's `CLAUDE_SECURESTORAGE_CONFIG_DIR`).
    ClaudeStore(PathBuf),
    /// A ChatGPT sign-in store directory (`eos-agent login chatgpt --chatgpt-store DIR`):
    /// OpenAI models on the user's ChatGPT plan.
    #[serde(rename = "openaiStore")]
    OpenAiStore(PathBuf),
    /// The name of an environment variable holding an API key.
    ApiKeyEnv(String),
    /// The name of an environment variable holding an OAuth bearer token.
    #[serde(rename = "authTokenEnv")]
    BearerEnv(String),
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Permissions {
    /// Every tool call goes to the client as `permission.decide`.
    #[default]
    Ask,
    Allow,
    /// No file writes, only read-only shell commands; nothing is asked.
    ReadOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Spawn {
    cwd: PathBuf,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) id: Option<String>,
    /// Continue the recorded conversation of `id`.
    #[serde(default)]
    pub(crate) resume: bool,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    append_system_prompt: Option<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    #[serde(default = "yes")]
    mcp: bool,
    /// The client's own MCP servers, in Claude Code's `mcpServers` shape.
    #[serde(default)]
    mcp_servers: Option<Value>,
    #[serde(default = "yes")]
    tool_search: bool,
    #[serde(default = "yes")]
    auto_compact: bool,
    #[serde(default)]
    compact_at: Option<u64>,
    #[serde(default = "yes")]
    claude_md: bool,
    #[serde(default)]
    plugin_dirs: Vec<PathBuf>,
    /// Stream summaries of the model's thinking as `thinkingDelta` events.
    #[serde(default = "yes")]
    thinking_summaries: bool,
    /// As Claude Code's interactive terminal (see `AgentOptions::interactive`).
    #[serde(default)]
    interactive: bool,
}

fn yes() -> bool {
    true
}

impl Spawn {
    pub(crate) fn options(self) -> Result<AgentOptions, RpcError> {
        let mut options = AgentOptions::new(self.cwd).mcp(self.mcp).roots(self.roots);
        options.model = self.model;
        options.effort = self.effort;
        options.id = self.id;
        options.allowed_tools = self.allowed_tools;
        options.disallowed_tools = self.disallowed_tools;
        options.max_tokens = self.max_tokens;
        options.context_window = self.context_window;
        options.thinking_summaries = self.thinking_summaries;
        options.interactive = self.interactive;
        options.tool_search = self.tool_search;
        options.auto_compact = self.auto_compact;
        options.compact_at = self.compact_at;
        options.claude_md = self.claude_md;
        options.plugin_dirs = self.plugin_dirs;
        if let Some(doc) = &self.mcp_servers {
            let config = McpConfig::from_json(doc);
            if let Some(bad) = config.errors.first() {
                return Err(RpcError::params(format!("mcpServers.{}: {}", bad.server, bad.message)));
            }
            options.mcp_servers = config.servers;
        }
        options.system_prompt = match (self.system_prompt, self.append_system_prompt) {
            (Some(_), Some(_)) => return Err(RpcError::params("give systemPrompt or appendSystemPrompt, not both")),
            (Some(text), None) => Some(SystemPromptOverride::Replace(text)),
            (None, Some(text)) => Some(SystemPromptOverride::Append(text)),
            (None, None) => None,
        };
        Ok(options)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Send {
    pub(crate) agent_id: String,
    /// What a user typed; `/name args` runs a skill or command.
    #[serde(default)]
    pub(crate) text: Option<String>,
    /// Content blocks sent as they are.
    #[serde(default)]
    pub(crate) content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentId {
    pub(crate) agent_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct McpServer {
    pub(crate) agent_id: String,
    pub(crate) server: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Notify {
    pub(crate) agent_id: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) wake: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Compact {
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) focus: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetModel {
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// Absent: unchanged. `null`: the model's default.
    #[serde(default, deserialize_with = "present")]
    pub(crate) effort: Option<Option<String>>,
}

/// A member that is present, even as `null`.
fn present<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(de).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Rewind {
    pub(crate) agent_id: String,
    pub(crate) keep: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StopTask {
    pub(crate) agent_id: String,
    pub(crate) task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Complete {
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) system: Option<String>,
    #[serde(default)]
    pub(crate) schema: Option<Value>,
    #[serde(default)]
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) max_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_takes_one_credential_or_a_list() {
        let one: Initialize = parse(Some(serde_json::json!({"auth": {"claudeStore": "/c"}}))).expect("one");
        assert!(matches!(one.auth.as_ref().map(Auths::all), Some([Auth::ClaudeStore(_)])));
        let both: Initialize =
            parse(Some(serde_json::json!({"auth": [{"claudeStore": "/c"}, {"openaiStore": "/o"}]}))).expect("list");
        assert!(matches!(both.auth.as_ref().map(Auths::all), Some([Auth::ClaudeStore(_), Auth::OpenAiStore(_)])));
        assert!(parse::<Initialize>(Some(serde_json::json!({"auth": {"nope": 1}}))).is_err());
    }

    #[test]
    fn spawn_is_not_interactive_unless_asked() {
        let spawn = |extra: serde_json::Value| {
            let mut params = serde_json::json!({"cwd": "/p"});
            params.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
            parse::<Spawn>(Some(params)).expect("spawn").options().expect("options")
        };
        assert!(!spawn(serde_json::json!({})).interactive);
        assert!(spawn(serde_json::json!({"interactive": true})).interactive);
    }
}
