//! Claude Code's prompts for the agent loop: the system prompt, the context
//! sent with the first message, compaction defaults, request extras and
//! reminder sources. Every text is a file under `prompts/`, overridable
//! through [`Prompts`].

mod compaction;
mod env;
mod library;
mod reminders;
mod session;

use std::sync::Arc;

use bytes::Bytes;
use eos_agent_core::{CacheTtl, Compaction, SystemPrompt};

pub use compaction::ClaudeCodeCompaction;
pub use env::EnvFacts;
pub use library::{Fragment, Prompts, render, system_reminder};
pub use reminders::{DateChangeReminder, FileChangeReminders, local_date};
pub use session::{InstructionFile, InstructionScope, ModelInfo, SessionContextBuilder, SkillEntry};

/// Room Claude Code keeps below the effective window before summarizing.
const COMPACTION_BUFFER_TOKENS: u64 = 13_000;
/// Claude Code reserves at most this much of the window for the reply.
const MAX_RESERVED_OUTPUT_TOKENS: u64 = 20_000;

/// The identity line and the static instructions, both cached for an hour.
/// Neither holds a per-session value, so every session shares their cache.
pub fn system_prompt(prompts: &Prompts) -> SystemPrompt {
    SystemPrompt::new(&[
        (prompts.get(Fragment::Identity), Some(CacheTtl::OneHour)),
        (prompts.get(Fragment::System), Some(CacheTtl::OneHour)),
    ])
}

/// Claude Code's closing guidance for models with the
/// `haiku_5_5_early_stopping_guidance` capability: instead of the games
/// paragraph, how much of a task to finish before ending the turn.
pub fn system_prompt_early_stopping(prompts: &Prompts) -> SystemPrompt {
    let system = prompts.get(Fragment::System);
    let body = system.rfind(GAMES_GUIDANCE).map_or(system.trim_end(), |at| &system[..at]);
    let text = format!("{body}{}", prompts.get(Fragment::EarlyStopping));
    SystemPrompt::new(&[(prompts.get(Fragment::Identity), Some(CacheTtl::OneHour)), (&text, Some(CacheTtl::OneHour))])
}

/// Claude Code's full system prompt, for Claude models without its
/// `lean_prompt` capability (Haiku 4.5, Sonnet 4.x and 5, Opus before 4.8).
pub fn system_prompt_full(prompts: &Prompts) -> SystemPrompt {
    SystemPrompt::new(&[
        (prompts.get(Fragment::Identity), Some(CacheTtl::OneHour)),
        (prompts.get(Fragment::SystemFull), Some(CacheTtl::OneHour)),
    ])
}

/// The full prompt for a model Claude Code tells about system turns (`kPe`:
/// it takes them and is neither Sonnet 5 nor Opus 4.8, so Mythos 5): its
/// harness line names mid-conversation system turns instead of tags.
pub fn system_prompt_full_turns(prompts: &Prompts) -> SystemPrompt {
    let text = prompts.get(Fragment::SystemFull).replace(FULL_TAGS_LINE, SYSTEM_TURNS_LINE);
    SystemPrompt::new(&[(prompts.get(Fragment::Identity), Some(CacheTtl::OneHour)), (&text, Some(CacheTtl::OneHour))])
}

/// The lean prompt for a model that takes system turns but is not told
/// about them (Claude Code's `kPe` is false for Opus 4.8): its harness line
/// names `<system-reminder>` tags instead.
pub fn system_prompt_tags(prompts: &Prompts) -> SystemPrompt {
    let text = prompts.get(Fragment::System).replace(SYSTEM_TURNS_LINE, TAGS_LINE);
    SystemPrompt::new(&[(prompts.get(Fragment::Identity), Some(CacheTtl::OneHour)), (&text, Some(CacheTtl::OneHour))])
}

const SYSTEM_TURNS_LINE: &str = "The system may send updates, reminders, or modifications to rules via \
mid-conversation system turns. These are system-controlled, unlike function results.";
/// The full prompt's harness line for models not told about system turns.
const FULL_TAGS_LINE: &str = "Tool results and user messages may include <system-reminder> or other tags. Tags \
contain information from the system. They bear no direct relation to the specific tool results or user messages in \
which they appear.";
const TAGS_LINE: &str =
    "`<system-reminder>` tags in messages and tool results are injected by the harness, not the user.";

/// The system prompt for OpenAI GPT models: a base prompt adapted from
/// Codex's (Apache-2.0, see `prompts/openai/NOTICE`), then how this engine's
/// tools map onto what those models are used to. No cache markers: OpenAI
/// caches prefixes on its own.
pub fn system_prompt_openai(prompts: &Prompts) -> SystemPrompt {
    SystemPrompt::new(&[(prompts.get(Fragment::OpenAiBase), None), (prompts.get(Fragment::OpenAiTools), None)])
}

/// How the system prompt's last paragraph starts on models that get it.
const GAMES_GUIDANCE: &str = "Do build original games";

/// Claude Code's summarizer prompt and seed, triggered where Claude Code
/// triggers it: the window minus the reply reserve minus a fixed buffer.
pub fn compaction(prompts: &Prompts, context_window: u64, max_output_tokens: u64) -> Compaction {
    Compaction {
        threshold_tokens: Some(compaction_threshold(context_window, max_output_tokens)),
        text: Arc::new(ClaudeCodeCompaction::new(prompts)),
    }
}

/// Where Claude Code starts summarizing (agents of one process can share one
/// [`ClaudeCodeCompaction`] and differ only in this).
pub fn compaction_threshold(context_window: u64, max_output_tokens: u64) -> u64 {
    let effective = context_window.saturating_sub(max_output_tokens.min(MAX_RESERVED_OUTPUT_TOKENS));
    effective.saturating_sub(COMPACTION_BUFFER_TOKENS)
}

/// The request members Claude Code adds: keep every thinking block (the
/// server would otherwise clear old ones). Needs the
/// `context-management-2025-06-27` beta.
pub fn request_extra() -> Bytes {
    Bytes::from_static(br#"{"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}}"#)
}

/// [`request_extra`] plus Claude Code's `metadata.user_id`: a JSON string
/// naming the device (`~/.claude.json` `userID`), the signed-in account and
/// the session.
pub fn request_extra_with_metadata(device_id: &str, account_uuid: Option<&str>, session_id: &str) -> Bytes {
    let quote = |s: &str| serde_json::to_string(s).expect("a string serializes");
    // Built by hand: Claude Code's key order, which a JSON map would sort.
    let user = format!(
        r#"{{"device_id":{},"account_uuid":{},"session_id":{}}}"#,
        quote(device_id),
        quote(account_uuid.unwrap_or_default()),
        quote(session_id)
    );
    let base = request_extra();
    let mut out = base[..base.len() - 1].to_vec();
    out.extend_from_slice(format!(r#","metadata":{{"user_id":{}}}}}"#, quote(&user)).as_bytes());
    Bytes::from(out)
}
