//! Reading a session Claude Code recorded (`~/.claude/projects/<project>/
//! <session>.jsonl`) as the conversation a resumed session would send the
//! model, so a chat started under Claude Code carries on here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eos_agent_core::StoredMessage;
use eos_agent_types::{ContentBlock, Role};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{ClaudePaths, project_dir_name};

/// Claude Code's stand-in result for a tool call that has none.
const MISSING_RESULT: &str = "[Tool result missing due to internal error]";
/// The model Claude Code names on messages it wrote itself (API errors).
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Where Claude Code keeps the transcript of `session_id`, started in `cwd`.
pub fn transcript_path(paths: &ClaudePaths, cwd: &Path, session_id: &str) -> PathBuf {
    paths.config_dir.join("projects").join(project_dir_name(cwd)).join(format!("{session_id}.jsonl"))
}

/// A Claude Code session's conversation, ready to continue: pass `messages`
/// as an agent's history.
#[derive(Debug, Clone, Default)]
pub struct ClaudeSession {
    /// The live conversation: the branch the session ended on, from its last
    /// compaction on, without subagents' sidechains and Claude Code's own
    /// bookkeeping entries, with the messages it delivered as attachments
    /// (see [`attachment_blocks`]). Every tool call has its result.
    pub messages: Vec<StoredMessage>,
    /// The model that wrote the last reply. Thinking blocks are signed for
    /// their model: see [`without_thinking`](Self::without_thinking).
    pub model: Option<String>,
}

/// One transcript line, as far as a conversation needs it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry<'a> {
    #[serde(rename = "type", default)]
    kind: &'a str,
    #[serde(default)]
    uuid: Option<&'a str>,
    #[serde(default)]
    parent_uuid: Option<&'a str>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_api_error_message: bool,
    #[serde(default)]
    is_visible_in_transcript_only: bool,
    #[serde(borrow, default)]
    message: Option<&'a RawValue>,
    #[serde(borrow, default)]
    attachment: Option<&'a RawValue>,
}

/// An `attachment` entry's attachment, as far as the conversation needs it.
#[derive(Deserialize)]
struct Attachment<'a> {
    #[serde(rename = "type", default)]
    kind: &'a str,
    #[serde(borrow, default)]
    prompt: Option<&'a RawValue>,
    #[serde(borrow, default)]
    messages: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct TeammateMessage {
    #[serde(default)]
    from: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(default)]
    id: Option<&'a str>,
    #[serde(default)]
    model: Option<&'a str>,
    #[serde(borrow)]
    content: &'a RawValue,
}

struct Turn {
    role: Role,
    /// An assistant message's API id: Claude Code writes one entry per
    /// content block, all with the id of the response they came in.
    id: Option<String>,
    blocks: Vec<ContentBlock>,
    /// Input Claude Code delivered as an attachment: a message of its own,
    /// as a note the engine delivers is.
    note: bool,
}

impl ClaudeSession {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        Ok(Self::parse(&std::fs::read(path)?))
    }

    /// The session in `jsonl` (lines that do not parse are skipped, as a
    /// line cut off by a crash is).
    pub fn parse(jsonl: &[u8]) -> Self {
        let entries: Vec<Entry<'_>> = jsonl
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Entry<'_>>(line).ok())
            .filter(|e| !e.is_sidechain)
            .collect();
        let by_uuid: HashMap<&str, usize> =
            entries.iter().enumerate().filter_map(|(i, e)| Some((e.uuid?, i))).collect();
        // The live branch: from the last message written (one delivered as an
        // attachment too), parent by parent. A compaction boundary has no
        // parent, so the walk ends there.
        let leaf = entries
            .iter()
            .rposition(|e| e.uuid.is_some() && (matches!(e.kind, "user" | "assistant") || !said(e).is_empty()));
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut at = leaf;
        while let Some(i) = at {
            if !seen.insert(i) {
                break;
            }
            chain.push(i);
            at = entries[i].parent_uuid.and_then(|p| by_uuid.get(p).copied());
        }
        chain.reverse();

        let mut turns: Vec<Turn> = Vec::new();
        let mut model = None;
        for entry in chain.into_iter().map(|i| &entries[i]) {
            let role = match entry.kind {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "attachment" => {
                    let blocks = said(entry);
                    if !blocks.is_empty() {
                        add(&mut turns, Turn { role: Role::User, id: None, blocks, note: true });
                    }
                    continue;
                }
                _ => continue,
            };
            if entry.is_api_error_message || entry.is_visible_in_transcript_only {
                continue;
            }
            let Some(message) = entry.message.and_then(|m| serde_json::from_str::<Message<'_>>(m.get()).ok()) else {
                continue;
            };
            if message.model == Some(SYNTHETIC_MODEL) {
                continue;
            }
            let blocks = blocks(message.content);
            if blocks.is_empty() {
                continue;
            }
            if role == Role::Assistant {
                model = message.model.map(str::to_owned).or(model);
            }
            add(&mut turns, Turn { role, id: message.id.map(str::to_owned), blocks, note: false });
        }
        pair_tool_results(&mut turns);
        let messages = turns.iter().filter_map(|t| StoredMessage::new(t.role, &t.blocks).ok()).collect();
        Self { messages, model }
    }

    /// The conversation without the model's thinking, for continuing on a
    /// model other than [`model`](Self::model) (thinking blocks carry a
    /// signature only their own model accepts).
    pub fn without_thinking(self) -> Vec<StoredMessage> {
        self.messages
            .into_iter()
            .filter_map(|message| {
                if message.role() != Role::Assistant {
                    return Some(message);
                }
                let mut blocks = message.blocks().ok()?;
                blocks.retain(|b| !matches!(b, ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }));
                StoredMessage::new(Role::Assistant, &blocks).ok()
            })
            .collect()
    }
}

/// What an `attachment` entry said to the model (nothing for other entries).
fn said(entry: &Entry<'_>) -> Vec<ContentBlock> {
    match entry.attachment {
        Some(raw) if entry.kind == "attachment" => attachment_blocks(raw),
        _ => Vec::new(),
    }
}

/// The conversation input Claude Code delivers as an attachment, as the
/// text the model read (it counts `queued_command`, `teammate_mailbox` and
/// `poll_events` as such input): a queued command's prompt (a message the
/// user sent while the model worked, a background task's or subagent's
/// `<task-notification>`, a teammate's `<teammate-message>`), as the engine
/// records a note it delivers itself, and a teammate mailbox's messages in
/// Claude Code's `<teammate-message>` form. Every other attachment is left
/// out: context a session here gets anew (CLAUDE.md, skills, deferred tools,
/// MCP instructions, plan mode, the date, reminders) or a snapshot of files
/// and the editor that the files on disk now stand for.
fn attachment_blocks(raw: &RawValue) -> Vec<ContentBlock> {
    let Ok(attachment) = serde_json::from_str::<Attachment<'_>>(raw.get()) else { return Vec::new() };
    match (attachment.kind, attachment.prompt, attachment.messages) {
        ("queued_command", Some(prompt), _) => blocks(prompt),
        ("teammate_mailbox", _, Some(messages)) => {
            let messages: Vec<TeammateMessage> = serde_json::from_str(messages.get()).unwrap_or_default();
            let text = messages.iter().map(teammate_message).collect::<Vec<_>>().join("\n\n");
            if text.is_empty() { Vec::new() } else { vec![ContentBlock::Text { text, citations: None }] }
        }
        _ => Vec::new(),
    }
}

fn teammate_message(message: &TeammateMessage) -> String {
    let attr = |v: &str| v.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
    let mut head = format!("<teammate-message teammate_id=\"{}\"", attr(&message.from));
    if let Some(color) = message.color.as_deref().filter(|c| !c.is_empty()) {
        head.push_str(&format!(" color=\"{}\"", attr(color)));
    }
    if let Some(summary) = message.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        head.push_str(&format!(" summary=\"{}\"", attr(summary)));
    }
    format!("{head}>\n{}\n</teammate-message>", message.text)
}

/// A message's content as blocks: a plain string is one text block; a
/// thinking block that never got its signature (a reply cut off) is left out.
fn blocks(content: &RawValue) -> Vec<ContentBlock> {
    let json = content.get();
    if json.starts_with('"') {
        return match serde_json::from_str::<String>(json) {
            Ok(text) if !text.is_empty() => vec![ContentBlock::Text { text, citations: None }],
            _ => Vec::new(),
        };
    }
    let mut blocks: Vec<ContentBlock> = serde_json::from_str(json).unwrap_or_default();
    blocks.retain(|b| match b {
        ContentBlock::Thinking { signature, .. } => !signature.is_empty(),
        ContentBlock::Text { text, .. } => !text.is_empty(),
        _ => true,
    });
    blocks
}

/// Joins `turn` to the conversation as the API wants it: the entries of one
/// response become one assistant message (also when a tool's result was
/// recorded between two of them), neighbouring user entries one user message
/// and neighbouring notes another, after it. A tool's result joins the
/// message that answers the calls, ahead of any note.
fn add(turns: &mut Vec<Turn>, turn: Turn) {
    if turn.role == Role::Assistant {
        // A conversation starts with the user.
        if turns.is_empty() {
            return;
        }
        let same = turns
            .iter()
            .rposition(|t| t.role == Role::Assistant)
            .filter(|&i| turn.id.is_some() && turns[i].id == turn.id);
        match same {
            Some(earlier) => turns[earlier].blocks.extend(turn.blocks),
            None => turns.push(turn),
        }
        return;
    }
    if !turn.note && turn.blocks.iter().any(is_result) {
        let run = turns.iter().rposition(|t| t.role != Role::User).map_or(0, |i| i + 1);
        match turns.get_mut(run) {
            Some(first) if !first.note => first.blocks.extend(turn.blocks),
            _ => turns.insert(run, turn),
        }
        return;
    }
    match turns.last_mut() {
        Some(last) if last.role == turn.role && last.note == turn.note => last.blocks.extend(turn.blocks),
        _ => turns.push(turn),
    }
}

fn is_result(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolResult { .. })
}

/// Makes every tool call have its result right after it, as the API
/// requires: a call that never got one (the session was cut off) gets Claude
/// Code's stand-in, and a result whose call is gone (cut by a compaction) is
/// dropped.
fn pair_tool_results(turns: &mut Vec<Turn>) {
    let mut i = 0;
    while i < turns.len() {
        let calls: Vec<String> = match turns[i].role {
            Role::Assistant => tool_uses(&turns[i].blocks),
            _ => Vec::new(),
        };
        let has_user_next = turns.get(i + 1).is_some_and(|t| t.role == Role::User && !t.note);
        if !calls.is_empty() && !has_user_next {
            turns.insert(i + 1, Turn { role: Role::User, id: None, blocks: Vec::new(), note: false });
        }
        if let Some(next) = turns.get_mut(i + 1).filter(|t| t.role == Role::User && !t.note) {
            let mut answered = HashSet::new();
            next.blocks.retain(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    calls.contains(tool_use_id) && answered.insert(tool_use_id.clone())
                }
                _ => true,
            });
            let missing = calls.iter().filter(|id| !answered.contains(*id)).map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: Some(eos_agent_types::ToolResultContent::Text(MISSING_RESULT.into())),
                is_error: true,
            });
            let mut blocks: Vec<ContentBlock> = missing.collect();
            blocks.append(&mut next.blocks);
            // Results first, as Claude Code orders a user message.
            blocks.sort_by_key(|b| !is_result(b));
            next.blocks = blocks;
        }
        i += 1;
    }
    // The first message answers no call.
    if let Some(first) = turns.first_mut() {
        first.blocks.retain(|b| !is_result(b));
    }
    turns.retain(|t| !t.blocks.is_empty());
}

fn tool_uses(blocks: &[ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Lines as Claude Code writes them: bookkeeping entries, one entry per
    /// assistant content block, sidecars beside tool results.
    fn jsonl(entries: &[Value]) -> Vec<u8> {
        entries.iter().map(|e| format!("{e}\n")).collect::<String>().into_bytes()
    }

    fn user(uuid: &str, parent: Option<&str>, content: Value) -> Value {
        json!({"parentUuid": parent, "isSidechain": false, "type": "user", "uuid": uuid, "sessionId": "s1",
               "cwd": "/repo", "version": "2.1.319", "timestamp": "2026-11-01T10:00:00.000Z", "userType": "external",
               "message": {"role": "user", "content": content}})
    }

    fn assistant(uuid: &str, parent: &str, id: &str, block: Value) -> Value {
        json!({"parentUuid": parent, "isSidechain": false, "type": "assistant", "uuid": uuid, "sessionId": "s1",
               "requestId": "req_1", "timestamp": "2026-11-01T10:00:01.000Z",
               "message": {"id": id, "type": "message", "role": "assistant", "model": "claude-opus-5-5",
                           "content": [block], "stop_reason": null, "stop_sequence": null,
                           "usage": {"input_tokens": 3, "output_tokens": 5}}})
    }

    fn result(uuid: &str, parent: &str, tool_use_id: &str, text: &str) -> Value {
        let mut entry =
            user(uuid, Some(parent), json!([{"tool_use_id": tool_use_id, "type": "tool_result", "content": text}]));
        entry["toolUseResult"] = json!({"stdout": text, "stderr": "", "interrupted": false});
        entry["sourceToolAssistantUUID"] = json!(parent);
        entry
    }

    fn tool_use(id: &str, name: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": {"command": "ls"}, "caller": {"type": "direct"}})
    }

    fn wire(session: &ClaudeSession) -> Vec<Value> {
        session.messages.iter().map(|m| serde_json::from_slice(m.json()).expect("json")).collect()
    }

    #[test]
    fn a_session_becomes_the_messages_the_api_takes() {
        let bytes = jsonl(&[
            json!({"type": "queue-operation", "operation": "enqueue", "sessionId": "s1", "content": "list the files"}),
            user("u1", None, json!("list the files")),
            json!({"type": "attachment", "uuid": "at1", "parentUuid": "u1", "isSidechain": false,
                   "attachment": {"type": "todo_reminder"}}),
            assistant("a1", "at1", "msg_1", json!({"type": "thinking", "thinking": "hm", "signature": "sig"})),
            assistant("a2", "a1", "msg_1", json!({"type": "text", "text": "Looking."})),
            assistant("a3", "a2", "msg_1", tool_use("toolu_1", "Bash")),
            // A subagent's sidechain, an api record and a title are not the conversation.
            json!({"type": "user", "uuid": "s1", "parentUuid": null, "isSidechain": true,
                   "message": {"role": "user", "content": "explore"}}),
            json!({"type": "api-request-blob", "hash": "h", "sessionId": "s1",
                   "message": {"role": "user", "content": [{"type": "text", "text": "x"}]}}),
            result("u2", "a3", "toolu_1", "a.txt"),
            json!({"type": "ai-title", "aiTitle": "Listing", "sessionId": "s1"}),
            assistant("a4", "u2", "msg_2", json!({"type": "text", "text": "One file: a.txt"})),
            json!({"type": "last-prompt", "lastPrompt": "list the files", "leafUuid": "a4", "sessionId": "s1"}),
        ]);
        let session = ClaudeSession::parse(&bytes);
        assert_eq!(session.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            wire(&session),
            [
                json!({"role": "user", "content": [{"type": "text", "text": "list the files"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hm", "signature": "sig"},
                    {"type": "text", "text": "Looking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                ]}),
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "One file: a.txt"}]}),
            ]
        );
        let plain = session.without_thinking();
        let second: Value = serde_json::from_slice(plain[1].json()).expect("json");
        assert_eq!(second["content"][0]["type"], "text");
    }

    #[test]
    fn it_starts_after_the_last_compaction_and_follows_the_live_branch() {
        let mut summary =
            user("u3", Some("b1"), json!("This session is being continued from a previous conversation."));
        summary["isCompactSummary"] = json!(true);
        summary["isVisibleInTranscriptOnly"] = json!(false);
        let bytes = jsonl(&[
            user("u1", None, json!("old question")),
            assistant("a1", "u1", "msg_1", json!({"type": "text", "text": "old answer"})),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null,
                   "logicalParentUuid": "a1", "isSidechain": false, "content": "Conversation compacted",
                   "compactMetadata": {"trigger": "auto", "preTokens": 150000}}),
            summary,
            assistant("a2", "u3", "msg_2", json!({"type": "text", "text": "carrying on"})),
            // The user went back and asked something else: `u4` is left behind.
            user("u4", Some("a2"), json!("abandoned")),
            assistant("a3", "u4", "msg_3", json!({"type": "text", "text": "abandoned answer"})),
            user("u5", Some("a2"), json!("the real follow-up")),
            assistant("a4", "u5", "msg_4", json!({"type": "text", "text": "the real answer"})),
        ]);
        let texts: Vec<String> = ClaudeSession::parse(&bytes).messages.iter().map(StoredMessage::text).collect();
        assert_eq!(
            texts,
            [
                "This session is being continued from a previous conversation.",
                "carrying on",
                "the real follow-up",
                "the real answer"
            ]
        );
    }

    #[test]
    fn tool_calls_always_have_their_results() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}});
        let mut error = assistant("e1", "u1", "err_1", json!({"type": "text", "text": "API Error: 529"}));
        error["isApiErrorMessage"] = json!(true);
        error["message"]["model"] = json!("<synthetic>");
        let bytes = jsonl(&[
            user("u1", None, json!([{"type": "text", "text": "look at this"}, image])),
            error,
            // Two calls of one response, the first result recorded between them.
            assistant("a1", "e1", "msg_1", tool_use("toolu_1", "Read")),
            result("r1", "a1", "toolu_1", "one"),
            assistant("a2", "r1", "msg_1", tool_use("toolu_2", "Read")),
            result("r2", "a2", "toolu_2", "two"),
            // A reply cut off while thinking, then a call whose result never came.
            assistant("a3", "r2", "msg_2", json!({"type": "thinking", "thinking": "half", "signature": ""})),
            assistant("a4", "a3", "msg_2", tool_use("toolu_3", "Bash")),
        ]);
        let session = ClaudeSession::parse(&bytes);
        let messages = wire(&session);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["content"][1], image, "an image goes back as it was written");
        let calls: Vec<&str> =
            messages[1]["content"].as_array().expect("blocks").iter().filter_map(|b| b["id"].as_str()).collect();
        assert_eq!(calls, ["toolu_1", "toolu_2"]);
        let results: Vec<&str> = messages[2]["content"]
            .as_array()
            .expect("blocks")
            .iter()
            .filter_map(|b| b["tool_use_id"].as_str())
            .collect();
        assert_eq!(results, ["toolu_1", "toolu_2"]);
        assert_eq!(
            messages[3]["content"],
            json!([{"type": "tool_use", "id": "toolu_3", "name": "Bash", "input": {"command": "ls"}}])
        );
        assert_eq!(
            messages[4]["content"],
            json!([{"type": "tool_result", "tool_use_id": "toolu_3", "content": MISSING_RESULT, "is_error": true}])
        );
    }

    #[test]
    fn broken_lines_and_empty_files_are_no_conversation() {
        assert!(ClaudeSession::parse(b"").messages.is_empty());
        let mut bytes = jsonl(&[user("u1", None, json!("hi"))]);
        bytes.extend_from_slice(br#"{"type":"assistant","uuid":"a1","parentUuid":"u1","mess"#);
        let session = ClaudeSession::parse(&bytes);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.model, None);
    }

    /// A session shaped like a long one a host compacted, with many tool
    /// calls and subagents: what it carries on is the live branch's own
    /// message content, never the bulk Claude Code keeps beside it.
    #[test]
    fn a_resumed_session_carries_only_its_live_conversation() {
        const STEPS: usize = 72;
        const SIGNATURE: usize = 4096;
        const PREVIEW: usize = 2000;
        let bulk = |tag: &str, kb: usize| format!("{tag} ").repeat(kb * 1024 / (tag.len() + 1));
        let mut lines = vec![
            user("p1", None, json!(bulk("PRECOMPACT", 200))),
            assistant("p2", "p1", "msg_p", json!({"type": "text", "text": bulk("PRECOMPACT", 200)})),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null,
                   "logicalParentUuid": "p2", "isSidechain": false}),
        ];
        let mut summary = user("s", Some("b1"), json!("This session is being continued from a previous conversation."));
        summary["isCompactSummary"] = json!(true);
        lines.push(summary);
        let mut parent = "s".to_owned();
        for n in 0..STEPS {
            let (progress, attachment, call) = (format!("g{n}"), format!("at{n}"), format!("toolu_{n}"));
            lines.push(json!({"type": "progress", "uuid": progress, "parentUuid": parent, "isSidechain": false,
                              "data": {"output": bulk("PROGRESS", 4)}}));
            lines.push(json!({"type": "attachment", "uuid": attachment, "parentUuid": progress, "isSidechain": false,
                              "attachment": {"type": "nested_memory", "content": bulk("ATTACHMENT", 8)}}));
            let id = format!("msg_{n}");
            let thinking = json!({"type": "thinking", "thinking": "", "signature": "S".repeat(SIGNATURE)});
            lines.push(assistant(&format!("t{n}"), &attachment, &id, thinking));
            let said = json!({"type": "text", "text": format!("Starting step {n}.")});
            lines.push(assistant(&format!("x{n}"), &format!("t{n}"), &id, said));
            lines.push(assistant(&format!("c{n}"), &format!("x{n}"), &id, tool_use(&call, "Agent")));
            lines.push(json!({"type": "user", "uuid": format!("sc{n}"), "parentUuid": null, "isSidechain": true,
                              "message": {"role": "user", "content": bulk("SIDECHAIN", 16)}}));
            let preview = format!("<persisted-output>{}</persisted-output>", "o".repeat(PREVIEW));
            let mut answer = result(&format!("r{n}"), &format!("c{n}"), &call, &preview);
            answer["toolUseResult"] = json!({"stdout": bulk("SIDECAR", 60)});
            lines.push(answer);
            parent = format!("r{n}");
        }
        // A question taken back: another branch from the same point.
        lines.push(user("ab", Some(&parent), json!(bulk("ABANDONED", 50))));
        lines.push(user("fin", Some(&parent), json!("and now?")));
        let bytes = jsonl(&lines);

        let session = ClaudeSession::parse(&bytes);
        let sent: Vec<String> =
            session.messages.iter().map(|m| String::from_utf8_lossy(m.json()).into_owned()).collect();
        let all = sent.concat();
        for decoy in ["PRECOMPACT", "PROGRESS", "ATTACHMENT", "SIDECHAIN", "SIDECAR", "ABANDONED"] {
            assert!(!all.contains(decoy), "{decoy} reached the conversation");
        }
        assert_eq!(session.messages.len(), 1 + 2 * STEPS, "the summary, each response and its result (+ question)");
        assert_eq!(all.matches(&"S".repeat(SIGNATURE)).count(), STEPS, "thinking stays, for the same model");
        // Each step: its signature, its result's preview and a little framing.
        assert!(all.len() < STEPS * (SIGNATURE + PREVIEW + 400), "{} bytes of {}", all.len(), bytes.len());
    }

    fn attachment(uuid: &str, parent: &str, attachment: Value) -> Value {
        json!({"parentUuid": parent, "isSidechain": false, "type": "attachment", "uuid": uuid, "sessionId": "s1",
               "timestamp": "2026-11-01T10:00:02.000Z", "attachment": attachment})
    }

    /// What Claude Code delivered as attachments and the model read: a
    /// background command's notification beside a tool result, a teammate's
    /// message, a message the user sent while the model worked (with its
    /// picture), a teammate mailbox, and a notification that came after the
    /// last reply. Each run of them is a user message of its own, as the
    /// engine records the notes it delivers; context attachments stay out;
    /// everything keeps its order.
    #[test]
    fn messages_delivered_as_attachments_stay_in_the_conversation() {
        let notification = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n\
                            <summary>Background command \"npm test\" completed (exit code 0)</summary>\n\
                            </task-notification>";
        let teammate = "<teammate-message teammate_id=\"researcher\" summary=\"found it\">\nThe bug is in a.rs\n\
                        </teammate-message>";
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}});
        let bytes = jsonl(&[
            user("u1", None, json!("run the tests in the background")),
            assistant("a1", "u1", "msg_1", tool_use("toolu_1", "Bash")),
            result("r1", "a1", "toolu_1", "Command running in background with ID: b1"),
            attachment(
                "q1",
                "r1",
                json!({"type": "queued_command", "prompt": notification,
                                          "commandMode": "task-notification", "origin": {"kind": "task-notification"},
                                          "isMeta": true, "source_uuid": "x1"}),
            ),
            attachment("n1", "q1", json!({"type": "nested_memory", "path": "/repo/CLAUDE.md", "content": "RULES"})),
            attachment("d1", "n1", json!({"type": "deferred_tools_delta", "addedNames": ["WebFetch"]})),
            assistant("a2", "d1", "msg_2", json!({"type": "text", "text": "Tests pass."})),
            attachment(
                "q2",
                "a2",
                json!({"type": "queued_command", "prompt": teammate,
                                          "origin": {"kind": "peer"}, "isMeta": true}),
            ),
            attachment(
                "q3",
                "q2",
                json!({"type": "queued_command", "commandMode": "prompt", "origin": {"kind": "human"},
                                          "prompt": [{"type": "text", "text": "also check b.rs"}, image]}),
            ),
            attachment(
                "m1",
                "q3",
                json!({"type": "teammate_mailbox", "messages": [
                {"from": "tester", "text": "all green", "timestamp": "t", "color": "blue", "summary": "tests \"ok\""}]}),
            ),
            attachment("e1", "m1", json!({"type": "edited_text_file", "filename": "/repo/a.rs", "snippet": "1\tfn a"})),
            assistant("a3", "e1", "msg_3", json!({"type": "text", "text": "Fixing a.rs and checking b.rs."})),
            attachment(
                "q4",
                "a3",
                json!({"type": "queued_command", "prompt": "<task-notification>late</task-notification>",
                                          "origin": {"kind": "task-notification"}}),
            ),
        ]);
        let messages = wire(&ClaudeSession::parse(&bytes));
        let mailbox = "<teammate-message teammate_id=\"tester\" color=\"blue\" summary=\"tests &quot;ok&quot;\">\n\
                       all green\n</teammate-message>";
        assert_eq!(
            messages,
            [
                json!({"role": "user", "content": [{"type": "text", "text": "run the tests in the background"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                ]}),
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1",
                     "content": "Command running in background with ID: b1"},
                ]}),
                json!({"role": "user", "content": [{"type": "text", "text": notification}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "Tests pass."}]}),
                json!({"role": "user", "content": [
                    {"type": "text", "text": teammate},
                    {"type": "text", "text": "also check b.rs"},
                    image,
                    {"type": "text", "text": mailbox},
                ]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "Fixing a.rs and checking b.rs."}]}),
                json!({"role": "user", "content": [{"type": "text", "text": "<task-notification>late</task-notification>"}]}),
            ]
        );
    }

    /// A note recorded between a call and its result, or after a call that
    /// never got one: the call is answered first, the note follows.
    #[test]
    fn a_tool_call_is_answered_before_a_note() {
        let note = |uuid: &str, parent: &str, text: &str| {
            attachment(uuid, parent, json!({"type": "queued_command", "prompt": text}))
        };
        let bytes = jsonl(&[
            user("u1", None, json!("go")),
            assistant("a1", "u1", "msg_1", tool_use("toolu_1", "Bash")),
            note("q1", "a1", "first note"),
            result("r1", "q1", "toolu_1", "done"),
            assistant("a2", "r1", "msg_2", tool_use("toolu_2", "Bash")),
            note("q2", "a2", "second note"),
        ]);
        let messages = wire(&ClaudeSession::parse(&bytes));
        let shapes: Vec<(String, String)> = messages
            .iter()
            .map(|m| {
                let first = &m["content"][0];
                let what = first["text"].as_str().or(first["tool_use_id"].as_str()).unwrap_or_default();
                (m["role"].as_str().unwrap_or_default().to_owned(), what.to_owned())
            })
            .collect();
        let shape = |role: &str, what: &str| (role.to_owned(), what.to_owned());
        assert_eq!(
            shapes,
            [
                shape("user", "go"),
                shape("assistant", ""),
                shape("user", "toolu_1"),
                shape("user", "first note"),
                shape("assistant", ""),
                shape("user", "toolu_2"),
                shape("user", "second note"),
            ]
        );
        assert_eq!(messages[5]["content"][0]["content"], MISSING_RESULT);
    }

    #[test]
    fn transcripts_are_found_where_claude_code_keeps_them() {
        let paths = ClaudePaths::for_home(Path::new("/Users/u"));
        assert_eq!(
            transcript_path(&paths, Path::new("/Users/u/my.repo"), "abc"),
            Path::new("/Users/u/.claude/projects/-Users-u-my-repo/abc.jsonl")
        );
    }
}
