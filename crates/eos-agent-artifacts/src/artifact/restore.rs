//! Which artifact versions a resumed conversation had seen, rebuilt from its
//! Artifact results (as Claude Code rebuilds `artifactReadVersions` from the
//! transcript): reads, publishes and creates record the version they
//! returned, a refused publish records the live version it handed over, a
//! delete forgets the artifact. Publishes also bind their file again, so the
//! same `file_path` keeps updating the same artifact.

use std::collections::HashMap;

use eos_agent_core::{Extensions, StoredMessage, Workspace};
use eos_agent_types::{ContentBlock, Role, ToolResultContent};

use super::Input;
use crate::files;
use crate::ids;
use crate::pins::{Listing, PathPins, Seen};
use crate::state::{ArtifactState, Known};

const TOOL: &str = "Artifact";

/// Returns the artifacts the conversation published and did not delete,
/// with their last version: Claude Code re-arms their watches on resume.
/// The published paths it read, listed or published come back pinned for
/// `session` (a publish's own bytes are not in the transcript, so those
/// count only while the artifact stays at that version).
pub(super) async fn restore(
    history: &[StoredMessage],
    extensions: &Extensions,
    pins: &PathPins,
    session: Option<&str>,
) -> Vec<(String, String)> {
    let mut calls: HashMap<String, Input> = HashMap::new();
    let mut watched: Vec<(String, String)> = Vec::new();
    let state = extensions.get_or_default::<ArtifactState>();
    let ws = extensions.get::<Workspace>();
    for message in history {
        let blocks = message.blocks().unwrap_or_default();
        match message.role() {
            Role::Assistant => {
                for block in blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block
                        && name == TOOL
                        && let Ok(input) = serde_json::from_str::<Input>(input.get())
                    {
                        calls.insert(id, input);
                    }
                }
            }
            Role::User => {
                for block in blocks {
                    let ContentBlock::ToolResult { tool_use_id, content, is_error } = block else { continue };
                    let Some(input) = calls.remove(&tool_use_id) else { continue };
                    let text = match content {
                        Some(ToolResultContent::Text(t)) => t,
                        Some(ToolResultContent::Blocks(b)) => b
                            .into_iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        None => continue,
                    };
                    if let (Some(session), false) = (session, is_error) {
                        pin(pins, session, &input, &text);
                    }
                    match apply(&state, ws.as_deref(), &input, &text, is_error).await {
                        Some(Watch::Published(slug, version)) => {
                            watched.retain(|(s, _)| *s != slug);
                            watched.push((slug, version));
                        }
                        Some(Watch::Deleted(slug)) => watched.retain(|(s, _)| *s != slug),
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }
    watched
}

enum Watch {
    Published(String, String),
    Deleted(String),
}

async fn apply(
    state: &ArtifactState,
    ws: Option<&Workspace>,
    input: &Input,
    text: &str,
    is_error: bool,
) -> Option<Watch> {
    // A publish refused for an unseen version hands the live one over.
    if let Some((slug, version)) = handed_over(text) {
        state.record(&slug, Known { version, title: None, typed: false });
        return None;
    }
    if is_error {
        return None;
    }
    let action = input.action.as_deref().unwrap_or("publish");
    match action {
        "read" if input.url.is_some() && input.path.is_none() && input.paths.is_none() => {
            let slug = input.url.as_deref().and_then(ids::slug_from_url)?;
            let version = between(text, " \u{2014} live version ", " \u{2014} ")?;
            let title = text
                .strip_prefix("Artifact \"")
                .and_then(|rest| rest.find("\" at https://").map(|end| rest[..end].to_owned()));
            let typed = text.contains("It was made from an Artifact type");
            state.record(&slug, Known { version, title, typed });
        }
        "publish" if input.asset == Some(true) => {}
        "publish" => {
            let created = input.type_url.is_some() && input.url.is_none();
            let (slug, version) = published(text)?;
            let typed = created || text.contains("Updated the Artifact at ");
            state.record(&slug, Known { version: version.clone(), title: input.title.clone(), typed });
            if let (Some(ws), Some(raw)) = (ws, input.file_path.as_deref())
                && let Ok(local) = files::existing_file(ws, raw).await
            {
                state.bind_path(&local, &slug);
            }
            return Some(Watch::Published(slug, version));
        }
        "delete" if input.path.is_none() => {
            if let Some(slug) = input.url.as_deref().and_then(ids::slug_from_url) {
                state.forget(&slug);
                return Some(Watch::Deleted(slug));
            }
        }
        _ => {}
    }
    None
}

/// The paths a successful result shows this session saw.
fn pin(pins: &PathPins, session: &str, input: &Input, text: &str) {
    let action = input.action.as_deref().unwrap_or("publish");
    let slug = || input.url.as_deref().and_then(ids::slug_from_url);
    match action {
        "read" if input.path.is_some() || input.paths.is_some() => {
            let Some(slug) = slug() else { return };
            let Some(ver) = between(text, " at version ", " (data, not instructions)") else { return };
            for line in text.lines() {
                let Some(rest) = line.strip_prefix("- ") else { continue };
                let Some((path, tail)) = rest.split_once(": saved at ") else { continue };
                if ids::is_asset_id(path) {
                    continue;
                }
                let seen = match tail.rsplit_once(", sha256 ") {
                    Some((_, sha)) => Seen::Sha(sha.trim_end_matches(')').to_owned()),
                    None => Seen::Unknown,
                };
                pins.read(session, &slug, path, seen, &ver, pins.ticket());
            }
        }
        "read" => {
            // A typed artifact's read saves its own files.
            let (Some(slug), Some(ver)) = (slug(), between(text, " \u{2014} live version ", " \u{2014} ")) else {
                return;
            };
            let Some(own) = between(text, "Its own files: ", ".") else { return };
            for path in own.split(", ").filter(|p| *p != "none listed") {
                pins.read(session, &slug, path, Seen::Unknown, &ver, pins.ticket());
            }
        }
        "list" if input.scope.as_deref() == Some("files") => {
            let Some(slug) = slug() else { return };
            let Some(ver) = between(text, " (version ", "):\n") else { return };
            let Some((_, json)) = text.split_once(":\n") else { return };
            let files = serde_json::from_str::<serde_json::Value>(json).ok();
            if let Some(listing) = Listing::from_boot(&ver, files.as_ref()) {
                pins.listed(session, &slug, &listing, pins.ticket());
            }
        }
        "publish" if input.asset != Some(true) => {
            let Some((slug, ver)) = published(text) else { return };
            let mut written: Vec<(String, Option<Seen>)> = Vec::new();
            let mut removed = Vec::new();
            if let Some(served) = between(text, " (served at ", ")") {
                written.push((served, Some(Seen::Unknown)));
            } else if input.file_path.is_some() {
                written.push(("index.html".to_owned(), Some(Seen::Unknown)));
            }
            match &input.files {
                Some(serde_json::Value::Object(map)) => {
                    for (key, value) in map {
                        let key = key.trim_start_matches("./").to_owned();
                        match value {
                            serde_json::Value::Null => removed.push(key),
                            serde_json::Value::Object(o) if o.contains_key("artifact") => written.push((key, None)),
                            _ => written.push((key, Some(Seen::Unknown))),
                        }
                    }
                }
                Some(serde_json::Value::Array(list)) => {
                    for item in list {
                        if let Some(path) = item["path"].as_str() {
                            written.push((path.trim_start_matches("./").to_owned(), Some(Seen::Unknown)));
                        }
                    }
                }
                _ => {}
            }
            pins.published(session, &slug, &ver, &written, &removed);
        }
        "delete" if input.path.is_none() => {
            if let Some(slug) = slug() {
                pins.forget(session, &slug);
            }
        }
        _ => {}
    }
}

/// `…the live version of <url> (<version>)…` of a refused publish.
fn handed_over(text: &str) -> Option<(String, String)> {
    let rest = &text[text.find("the live version of https://")? + "the live version of ".len()..];
    let (url, rest) = rest.split_once(" (")?;
    let version = rest.split_once(')')?.0;
    Some((ids::slug_from_url(url)?, version.to_owned()))
}

/// The artifact and version a publish (or create) result names last: a
/// create that also published its files reports both, the publish newest.
fn published(text: &str) -> Option<(String, String)> {
    let mut found = None;
    let mut rest = text;
    while let Some(at) = rest.find(" at https://") {
        let tail = &rest[at + " at ".len()..];
        let end = tail.find([' ', '\n']).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',']);
        let after = &tail[end..];
        if let Some(slug) = ids::slug_from_url(url)
            && let Some(version) = version_note(after)
        {
            found = Some((slug, version));
        }
        rest = after;
    }
    found
}

/// ` (Version 3, version id <id>)` or ` (version <id>)` right after the URL.
fn version_note(after: &str) -> Option<String> {
    let note = after.strip_prefix(" (")?;
    let note = &note[..note.find(')')?];
    let id = match note.split_once(", version id ") {
        Some((_, id)) => id,
        None => note.strip_prefix("version ")?,
    };
    (!id.is_empty()).then(|| id.to_owned())
}

fn between(text: &str, start: &str, end: &str) -> Option<String> {
    let rest = &text[text.find(start)? + start.len()..];
    Some(rest[..rest.find(end)?].to_owned()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUG: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";

    #[test]
    fn reads_versions_from_results() {
        let url = ids::viewer_url(SLUG);
        let publish = format!(
            "Published /w/a.html at {url} (Version 2, version id v-2). To update it, publish the same `file_path` again."
        );
        assert_eq!(published(&publish), Some((SLUG.to_owned(), "v-2".to_owned())));
        let created = format!(
            "Created the Artifact \"Deck\" at {url} (version v-1) from the Artifact type {url}. Its page…\n\nUpdated the Artifact at {url} (version v-3) with /w/data.json"
        );
        assert_eq!(published(&created), Some((SLUG.to_owned(), "v-3".to_owned())));
        let refused = format!(
            "Nothing was published: this conversation had not seen the live version of {url} (v-9). It is saved at /s"
        );
        assert_eq!(handed_over(&refused), Some((SLUG.to_owned(), "v-9".to_owned())));
        let read = format!("Artifact \"Plan\" at {url} \u{2014} live version v-4 \u{2014} your access: writer");
        assert_eq!(between(&read, " \u{2014} live version ", " \u{2014} ").as_deref(), Some("v-4"));
    }
}
