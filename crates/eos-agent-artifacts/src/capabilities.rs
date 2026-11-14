//! The text of Claude Code's bundled `artifact-capabilities` skill. Claude
//! Code ships no file for it: each load composes it from the frame API's
//! runtime contract (the capability roster, the contract's authoring guide
//! and the `window.claude` type definitions, which are written to the skill's
//! folder for the agent to Read), plus what this session offers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::Value;

use eos_agent_core::Extensions;

use crate::client::{FrameClient, FrameError, RequestBody};
use crate::targets::{ContractTargets, session_of, valid_pin};

const HEADER: &str = include_str!("../prompts/capabilities/header.md");
/// Connector guidance when the contract lets pages call local MCP servers.
const CONNECTORS_HOST: &str = include_str!("../prompts/capabilities/connectors-host.md");
const CONNECTORS: &str = include_str!("../prompts/capabilities/connectors.md");
const MCP_SHAPES: &str = include_str!("../prompts/capabilities/mcp-shapes.md");
const READ_HINT: &str = "Open these files with the Read tool rather than `cat`: a file past the Bash tool's inline \
output limit does not come back in full.";
const NONE: &str = "**No runtime capabilities are available to you for this artifact.** Do not declare or guess any \
`capabilities` name; if the user asked for one, say it is unavailable and build a static page.";
const UNREACHABLE: &str = "_(The current contract's capability roster could not be fetched; the contract service may \
be unreachable \u{2014} invoking this skill again retries.)_";
/// How the steps of the contract's guide name the ArtifactData tool here.
const TOOL_SPELLING: &str = "> Tool spelling in this session: the `Artifact` tool's `action: \"read_db\"` / \
`\"write_db\"` with a `db_op` are the `ArtifactData` tool, whose `action` is that `db_op` (\"get\", \"list\", \
\"query\", \"set\", \"update\", \"str_replace\", \"delete\", \"batch\") with the other fields unchanged \u{2014} load \
it with ToolSearch when you first need it. Read the steps below with that substitution.\n\n";
/// The definitions that tell how a page reaches any capability.
const CORE_DEFS: &str = "claude";

const ROSTER_TIMEOUT: Duration = Duration::from_secs(5);
/// Claude Code's wait for a target's pin read-back (`Tn`).
const PIN_TIMEOUT: Duration = Duration::from_secs(5);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFS_TIMEOUT: Duration = Duration::from_secs(5);
const DEFS_DEADLINE: Duration = Duration::from_secs(15);
const MAX_PROMPT_BYTES: usize = 64 << 10;
const MAX_DEFS_BYTES: usize = 256 << 10;

/// Composes the `artifact-capabilities` skill for one load.
#[derive(Clone)]
pub struct CapabilitiesGuide {
    client: Arc<FrameClient>,
    dir: PathBuf,
    targets: Arc<ContractTargets>,
}

impl std::fmt::Debug for CapabilitiesGuide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilitiesGuide").field("dir", &self.dir).finish_non_exhaustive()
    }
}

/// The runtime contract as `contract/latest` (or a pinned version) names it.
struct Roster {
    version: String,
    capabilities: Vec<String>,
    core: Vec<String>,
    /// The contract serves `claude.d.ts` (how a page reaches capabilities).
    claude: bool,
    /// Pages may call local MCP servers (`features.mcp.host`).
    host_servers: bool,
    /// The target artifact whose pinned contract this is.
    pinned: Option<String>,
}

/// What one load fetched beyond the roster.
struct Fetched {
    /// Type definitions written to the skill folder, relative to it, sorted.
    files: Vec<String>,
    /// The contract's authoring guide.
    prompt: Option<String>,
    /// Capabilities the guide does not cover (its `x-contract-missing-caps`).
    missing: Vec<String>,
}

impl CapabilitiesGuide {
    /// `dir` is the skill's folder (the type definitions go under it).
    pub fn new(client: Arc<FrameClient>, dir: PathBuf) -> Self {
        Self { client, dir, targets: Arc::default() }
    }

    /// The table of what each session works on, which the `Artifact` tool
    /// keeps up to date.
    pub(crate) fn targets(&self) -> Arc<ContractTargets> {
        Arc::clone(&self.targets)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The skill's text for no session in particular (the latest contract).
    pub async fn render(&self) -> String {
        self.render_for(None).await
    }

    /// The skill's text, beginning with its base directory when type
    /// definitions were written there. For an agent (its extensions and id)
    /// whose session targets an artifact pinned to a contract, Claude Code's
    /// guidance for that contract.
    pub async fn render_for(&self, call: Option<(&Extensions, &str)>) -> String {
        let session = call.map(|(extensions, agent)| session_of(extensions, agent));
        let pin = match &session {
            Some(session) => self.pin(session).await,
            None => None,
        };
        let version = pin.as_ref().map_or("latest", |(_, v)| v.as_str());
        let Some(mut roster) = self.roster(version).await else { return join(&[HEADER.trim_end(), UNREACHABLE]) };
        roster.pinned = pin.map(|(slug, _)| slug);
        if roster.capabilities.is_empty() {
            return join(&[HEADER.trim_end(), NONE]);
        }
        let mut wanted: Vec<&str> = roster.capabilities.iter().map(String::as_str).collect();
        if roster.claude {
            wanted.insert(0, CORE_DEFS);
        }
        let (files, (prompt, missing)) =
            tokio::join!(self.defs(&roster.version, &wanted), self.prompt(&roster.version));
        let missing = missing.into_iter().filter(|m| roster.capabilities.contains(m)).collect();
        let text =
            compose(&roster, &Fetched { files: files.clone(), prompt, missing }, &self.dir.display().to_string());
        if files.is_empty() { text } else { format!("Base directory for this skill: {}\n\n{text}", self.dir.display()) }
    }

    /// Claude Code's pin lookup: the session's target artifact and its pinned
    /// contract — noted, or read back once (`GET read/<slug>`, its
    /// `contract`; an artifact with none is not asked again).
    async fn pin(&self, session: &str) -> Option<(String, String)> {
        let target = self.targets.get(session);
        let slug = target.slug?;
        if let Some(pin) = target.pins.get(&slug) {
            return valid_pin(Some(pin)).map(|v| (slug, v));
        }
        if self.targets.is_unpinned(&slug) {
            return None;
        }
        match tokio::time::timeout(PIN_TIMEOUT, self.client.read_meta(&slug)).await.ok()? {
            Ok(meta) => match valid_pin(meta["contract"].as_str()) {
                Some(version) => {
                    self.targets.set(session, &slug, Some(&version), false);
                    Some((slug, version))
                }
                None => {
                    self.targets.mark_unpinned(&slug);
                    None
                }
            },
            Err(FrameError::Status { status: 404, .. }) => {
                self.targets.mark_unpinned(&slug);
                None
            }
            Err(_) => None,
        }
    }

    async fn roster(&self, version: &str) -> Option<Roster> {
        let value = tokio::time::timeout(ROSTER_TIMEOUT, self.client.contract(version)).await.ok()?.ok()?;
        let version = value["version"].as_str().filter(|v| valid_version(v))?.to_owned();
        let names = |key: &str| -> Vec<String> {
            value[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|n| valid_name(n))
                .map(str::to_owned)
                .collect()
        };
        Some(Roster {
            version,
            capabilities: names("capabilities"),
            core: names("core"),
            claude: value["claude"] == Value::Bool(true),
            host_servers: value["features"]["mcp"].get("host").is_some(),
            pinned: None,
        })
    }

    /// Fetches and writes `<version>/<name>.d.ts` for each name, within one
    /// overall deadline; the written ones, sorted.
    async fn defs(&self, version: &str, names: &[&str]) -> Vec<String> {
        let deadline = Instant::now() + DEFS_DEADLINE;
        let mut files = Vec::new();
        for name in names {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let path = format!("/api/frame/contract/{version}/{name}.d.ts");
            let Some(text) = self.text(&path, left.min(DEFS_TIMEOUT), MAX_DEFS_BYTES).await.map(|(t, _)| t) else {
                continue;
            };
            let rel = format!("{version}/{name}.d.ts");
            let dest = self.dir.join(&rel);
            let written = async {
                tokio::fs::create_dir_all(dest.parent()?).await.ok()?;
                tokio::fs::write(&dest, text).await.ok()
            };
            if written.await.is_some() {
                files.push(rel);
            }
        }
        files.sort();
        files
    }

    async fn prompt(&self, version: &str) -> (Option<String>, Vec<String>) {
        let path = format!("/api/frame/contract/{version}/prompt");
        let Some((text, missing)) = self.text(&path, PROMPT_TIMEOUT, MAX_PROMPT_BYTES).await else {
            return (None, Vec::new());
        };
        let missing = missing
            .map(|h| h.chars().take(4096).collect::<String>())
            .map(|h| h.split(',').map(str::trim).filter(|n| valid_name(n)).map(str::to_owned).collect())
            .unwrap_or_default();
        (Some(text), missing)
    }

    /// A non-empty text answer (and its missing-capabilities header).
    async fn text(&self, path: &str, limit: Duration, max: usize) -> Option<(String, Option<String>)> {
        let call = self.client.call_raw(Method::GET, path, &RequestBody::Empty, &[]);
        let (status, headers, bytes) = tokio::time::timeout(limit, call).await.ok()?.ok()?;
        if !(200..300).contains(&status) || bytes.len() > max {
            return None;
        }
        let text = String::from_utf8(bytes.to_vec()).ok().filter(|t| !t.trim().is_empty())?;
        let missing = headers.get("x-contract-missing-caps").and_then(|v| v.to_str().ok()).map(str::to_owned);
        Some((text, missing))
    }
}

fn compose(roster: &Roster, fetched: &Fetched, dir: &str) -> String {
    let mut parts = vec![HEADER.trim_end().to_owned(), available(&roster.capabilities, &roster.core)];
    if let Some(slug) = &roster.pinned {
        parts.push(format!(
            "_This guidance is pinned to runtime contract {} \u{2014} the contract the target artifact `{slug}` \
currently runs. A carry-forward republish keeps this pin._",
            roster.version
        ));
    }
    if let Some(prompt) = &fetched.prompt {
        parts.push(format!("{TOOL_SPELLING}{prompt}"));
    }
    let uncovered = if fetched.prompt.is_none() { &roster.capabilities } else { &fetched.missing };
    for name in uncovered {
        let suffix = format!("/{name}.d.ts");
        parts.push(match fetched.files.iter().find(|f| f.ends_with(&suffix)) {
            Some(file) => {
                let verb = if roster.core.contains(name) { "calling" } else { "declaring" };
                format!(
                    "**`{name}`.** Its authoring guidance could not be fetched this invocation; its type definitions \
are extracted at `{dir}/{file}` \u{2014} Read that file before {verb} this capability."
                )
            }
            None => format!(
                "**`{name}`.** Its authoring guidance and type definitions could not be fetched this invocation \
\u{2014} invoking this skill again retries."
            ),
        });
    }
    let core_file = fetched.files.iter().find(|f| f.ends_with(&format!("/{CORE_DEFS}.d.ts")));
    if roster.capabilities.iter().any(|c| c == "mcp") {
        let connectors = if roster.host_servers { CONNECTORS_HOST } else { CONNECTORS };
        parts.push(format!("**Your connectors this session.** {}", connectors.trim_end()));
        parts.push(call_contract(&roster.version, &fetched.files, core_file, dir));
    } else if !fetched.files.is_empty() {
        let others = fetched.files.iter().any(|f| Some(f) != core_file);
        let core = core_file.map_or(String::new(), |f| {
            format!(
                " `{f}` documents how a page reaches any capability on this contract \u{2014} Read it{}.",
                if others { " first" } else { "" }
            )
        });
        let each = if others {
            format!(
                " {} file documents its own declaration config and runtime surface \u{2014} Read it before declaring \
that capability.",
                if core_file.is_some() { "Each capability's" } else { "Each" }
            )
        } else {
            String::new()
        };
        parts.push(format!(
            "**Type definitions.** Extracted under `{dir}`: {}.{core}{each} {READ_HINT}",
            ticked(&fetched.files)
        ));
    }
    parts.extend(session_sections(&roster.capabilities));
    join(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The roster line: what may be declared, and what every page has built in.
fn available(roster: &[String], core: &[String]) -> String {
    let built_in: Vec<String> = roster.iter().filter(|c| core.contains(c)).cloned().collect();
    if built_in.is_empty() {
        return format!(
            "**Available capabilities:** {} \u{2014} the complete set of capability names you may declare. Anything \
not listed is unavailable to this user.",
            ticked(roster)
        );
    }
    let declarable: Vec<String> = roster.iter().filter(|c| !core.contains(c)).cloned().collect();
    let declare = if declarable.is_empty() {
        "none to declare for this user; ".to_owned()
    } else {
        format!("{} \u{2014} the complete set of capability names you may declare; ", ticked(&declarable))
    };
    format!(
        "**Available capabilities:** {declare}built in on every page, called without declaring (never pass these in \
`capabilities`): {}. Anything not listed is unavailable to this user.",
        ticked(&built_in)
    )
}

fn call_contract(version: &str, files: &[String], core: Option<&String>, dir: &str) -> String {
    let Some(mcp) = files.iter().find(|f| f.ends_with("/mcp.d.ts")) else {
        let core = core.map_or(String::new(), |c| {
            format!(" `{dir}/{c}` (how a page reaches any capability on this contract) did extract \u{2014} Read it.")
        });
        return format!(
            "**Call contract.** The served `mcp` type definitions could not be extracted for this invocation \u{2014} \
invoking this skill again retries. Do not write `mcp` capability calls from memory; the served definitions are the \
authority.{core} {}",
            MCP_SHAPES.trim_end()
        );
    };
    let read = match core {
        Some(c) => format!(
            "Read `{dir}/{c}` (how a page reaches any capability on this contract) and `{dir}/{mcp}` before writing \
any code that calls the `mcp` capability \u{2014} they are"
        ),
        None => format!("Read `{dir}/{mcp}` before writing any code that calls the `mcp` capability \u{2014} it is"),
    };
    format!(
        "**Call contract** (runtime contract {version}). The platform-served `window.claude` type definitions for \
this contract are extracted under `{dir}`: {}. {read} authoritative for this contract version over any remembered API \
shape. {READ_HINT} {}",
        ticked(files),
        MCP_SHAPES.trim_end()
    )
}

/// Where a page keeps its state, and the check before handing over the link,
/// for a session with the ArtifactData tool.
fn session_sections(roster: &[String]) -> Vec<String> {
    let has = |c: &str| roster.iter().any(|r| r == c);
    let page_record = has("artifact") || has("self");
    let mut out = Vec::new();
    if page_record || has("db") {
        let mut lines = vec![
            "- A per-viewer convenience (a remembered tab, a draft): browser storage; it never reaches other viewers \
or Claude.",
        ];
        if page_record {
            lines.push(
                "- The page itself is the record (a poll, a sign-up sheet, a checklist): the `artifact` capability \
\u{2014} a viewer who can write republishes the whole page from its state; every open view reloads to the winner, a \
concurrent save rejects `conflict`, and read-only viewers cannot save. Such a page regenerates the whole document from \
its state: keep the head, tokens and structure and change only the content.",
            );
        }
        if has("db") {
            lines.push(
                "- Data outside the page (Claude seeds or reads it, more than the page shows, private per viewer, many \
writers at once): the `db` capability \u{2014} documents under access rules, live through `onSnapshot`, kept across \
republishes.",
            );
        }
        out.push(format!("## Where a page keeps its state \u{2014} this session\n\n{}", lines.join("\n")));
    }
    if has("db") {
        out.push(
            "## Verify before you hand over the link \u{2014} this session\n\nA page whose `capabilities` you declared \
in this session gets one functional pass, not a render loop: after the first publish, one `ArtifactData` `list` of each \
collection the page writes, and, where its rules hide something from ordinary viewers, the same read with a lower \
`as_level`, which must not show what the rules hide from such a viewer. Then tell the user in one line what you \
exercised and what you could not. An Artifact made from an Artifact type is not such a page: its capabilities come from \
the type, and the type's instructions govern any checking."
                .to_owned(),
        );
    }
    out
}

fn ticked(names: &[String]) -> String {
    names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
}

fn join(parts: &[&str]) -> String {
    parts.join("\n\n")
}

/// `major.minor.patch`, as the contract names its versions.
pub(crate) fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts.iter().zip([4usize, 5, 6]).all(|(p, max)| {
            !p.is_empty()
                && p.len() <= max
                && p.bytes().all(|b| b.is_ascii_digit())
                && (*p == "0" || !p.starts_with('0'))
        })
}

/// A capability name: a lowercase letter, then up to 23 letters or digits.
fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 24
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(caps: &[&str], core: &[&str]) -> Roster {
        Roster {
            version: "0.2.96".into(),
            capabilities: caps.iter().map(|s| (*s).to_owned()).collect(),
            core: core.iter().map(|s| (*s).to_owned()).collect(),
            claude: true,
            host_servers: true,
            pinned: None,
        }
    }

    #[test]
    fn the_roster_line_splits_built_in_capabilities() {
        let caps: Vec<String> = ["db", "permissions"].map(String::from).to_vec();
        assert_eq!(
            available(&caps, &["permissions".into()]),
            "**Available capabilities:** `db` \u{2014} the complete set of capability names you may declare; built in \
on every page, called without declaring (never pass these in `capabilities`): `permissions`. Anything not listed is \
unavailable to this user."
        );
        assert!(
            available(&caps, &[]).starts_with("**Available capabilities:** `db`, `permissions` \u{2014} the complete")
        );
        assert!(valid_version("0.2.96") && !valid_version("0.02.1") && !valid_version("1.2"));
        assert!(valid_name("db") && !valid_name("Db") && !valid_name("1x"));
    }

    #[test]
    fn composes_the_sections_in_claude_codes_order() {
        let files = vec!["0.2.96/claude.d.ts".to_owned(), "0.2.96/db.d.ts".to_owned(), "0.2.96/mcp.d.ts".to_owned()];
        let r = roster(&["artifact", "db", "mcp", "permissions"], &["permissions"]);
        let text = compose(&r, &Fetched { files: files.clone(), prompt: Some("GUIDE".into()), missing: vec![] }, "/d");
        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} in {text}"));
        assert!(at("# Artifact runtime capabilities") < at("**Available capabilities:**"));
        assert!(at("**Available capabilities:**") < at("> Tool spelling in this session"));
        assert!(at("ToolSearch when you first need it. Read the steps below with that substitution.\n\nGUIDE") > 0);
        assert!(at("GUIDE") < at("**Your connectors this session.** None are connected right now"));
        assert!(at("host:<server>") < at("**Call contract** (runtime contract 0.2.96)"));
        assert!(text.contains(
            "Read `/d/0.2.96/claude.d.ts` (how a page reaches any capability on this contract) and `/d/0.2.96/mcp.d.ts`"
        ));
        assert!(at("**Call contract**") < at("## Where a page keeps its state"));
        assert!(at("## Where a page keeps its state") < at("## Verify before you hand over the link"));

        // Without the guide, every capability says so; without `mcp`, the files are listed.
        let r = roster(&["db"], &[]);
        let text = compose(&r, &Fetched { files: vec!["0.2.96/db.d.ts".into()], prompt: None, missing: vec![] }, "/d");
        assert!(text.contains("**`db`.** Its authoring guidance could not be fetched this invocation; its type definitions are extracted at `/d/0.2.96/db.d.ts` \u{2014} Read that file before declaring this capability."));
        assert!(text.contains("**Type definitions.** Extracted under `/d`: `0.2.96/db.d.ts`. Each file documents"));
    }

    /// Claude Code 2.1.319 builds its live-room section only when its room
    /// feature is on (`xat()`), and that feature is compiled out of the
    /// build (`AQ()` reads `null?.isArtifactRoomEnabled()`): a roster naming
    /// `room` gets no such section. With no claude.ai connector loaded, the
    /// connector section is its text for none.
    #[test]
    fn rooms_add_nothing_and_no_connectors_reads_as_none() {
        let r = Roster { host_servers: false, ..roster(&["mcp", "room"], &[]) };
        let text = compose(&r, &Fetched { files: vec![], prompt: Some("GUIDE".into()), missing: vec![] }, "/d");
        assert!(!text.contains("Live room"), "{text}");
        assert!(text.contains(
            "**Your connectors this session.** None are connected right now \u{2014} they may still be connecting, or \
the user has none. Look for tools prefixed `mcp__claude_ai_*` in your tool list; each is named \
`mcp__claude_ai_<connector>__<tool>`. Only claude.ai connectors are valid \u{2014} locally-configured MCP servers are \
not."
        ));
        let pinned = Roster { pinned: Some("s1".into()), ..roster(&["db"], &[]) };
        let text = compose(&pinned, &Fetched { files: vec![], prompt: None, missing: vec![] }, "/d");
        assert!(text.contains(
            "\n\n_This guidance is pinned to runtime contract 0.2.96 \u{2014} the contract the target artifact `s1` \
currently runs. A carry-forward republish keeps this pin._\n\n"
        ));
    }
}
