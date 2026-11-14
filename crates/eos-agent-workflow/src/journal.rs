//! A run's journal (`journal.jsonl` in its transcript directory): which
//! `agent()` calls started and what each returned, so a resumed run replays
//! the unchanged ones from it instead of spawning them again.
//!
//! Keys are Claude Code's: each call's key hashes its prompt and key
//! options with the previous key on the same async lane, so a key names a
//! call's position in its branch of the script. `parallel()` and
//! `pipeline()` fork a lane into one per slot and join them after; the run
//! keeps the lanes ([`Lanes`]), the rewritten script tells it which lane a
//! call came from (see `engine/rewrite.rs`). A resumed run replays until the
//! first call with no cached result, then runs everything live.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

pub const FILE: &str = "journal.jsonl";
/// Slots one fork may have: the boundary's array cap.
pub const MAX_SLOTS: usize = 4096;
const RESUMED: &str = r#"{"type":"resumed"}"#;
/// The options that make two calls the same call (label and phase do not).
const KEY_OPTIONS: [&str; 7] =
    ["schema", "model", "effort", "isolation", "agentType", "disallowedTools", "bashCommandClamp"];

/// One journal line. Results stay JSON text, so a replayed object keeps the
/// key order its agent produced.
#[derive(Debug, Clone)]
pub enum Entry {
    Launched,
    Started { key: String, agent_id: String, label: Option<String>, phase: Option<String> },
    Result { key: String, agent_id: String, result: Box<RawValue>, tools_used: Option<Vec<String>> },
    Failed { key: String, agent_id: String },
    Resumed,
}

/// The line as written: a flat object with a `type`.
#[derive(Serialize, Deserialize)]
struct Line<'a> {
    #[serde(rename = "type")]
    kind: std::borrow::Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<RawValue>>,
    #[serde(rename = "toolsUsed", skip_serializing_if = "Option::is_none")]
    tools_used: Option<Vec<String>>,
}

impl Entry {
    fn to_line(&self) -> String {
        let blank = |kind: &'static str| Line {
            kind: kind.into(),
            key: None,
            agent_id: None,
            label: None,
            phase: None,
            result: None,
            tools_used: None,
        };
        let line = match self.clone() {
            Self::Launched => blank("launched"),
            Self::Resumed => blank("resumed"),
            Self::Started { key, agent_id, label, phase } => {
                Line { key: Some(key), agent_id: Some(agent_id), label, phase, ..blank("started") }
            }
            Self::Result { key, agent_id, result, tools_used } => {
                Line { key: Some(key), agent_id: Some(agent_id), result: Some(result), tools_used, ..blank("result") }
            }
            Self::Failed { key, agent_id } => Line { key: Some(key), agent_id: Some(agent_id), ..blank("failed") },
        };
        serde_json::to_string(&line).unwrap_or_default()
    }

    fn from_line(text: &str) -> Option<Self> {
        let line: Line<'_> = serde_json::from_str(text).ok()?;
        let key = || line.key.clone().filter(|k| valid_key(k));
        Some(match line.kind.as_ref() {
            "launched" => Self::Launched,
            "resumed" => Self::Resumed,
            "started" => Self::Started { key: key()?, agent_id: line.agent_id?, label: line.label, phase: line.phase },
            "result" => Self::Result {
                key: key()?,
                agent_id: line.agent_id.unwrap_or_default(),
                result: line.result?,
                tools_used: line.tools_used,
            },
            "failed" => Self::Failed { key: key()?, agent_id: line.agent_id.unwrap_or_default() },
            _ => return None,
        })
    }
}

/// `v<n>:` and 64 hex digits.
fn valid_key(key: &str) -> bool {
    let Some((version, hash)) = key.split_once(':') else { return false };
    version.len() > 1
        && version.starts_with('v')
        && version[1..].bytes().all(|b| b.is_ascii_digit())
        && hash.len() == 64
        && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A journaled result and the agent that produced it.
#[derive(Debug, Clone)]
pub struct Cached {
    pub result: Box<RawValue>,
    pub agent_id: String,
}

/// What a journal says about each key (Claude Code's `oo`).
#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub results: HashMap<String, Cached>,
    pub started: HashMap<String, usize>,
    pub failed: HashSet<String>,
    /// Had a result, then started again: no longer trusted.
    pub demoted: HashSet<String>,
    /// Got a result again after one, a demotion or a failure: the lane goes
    /// on from [`replaced`] of the latest result.
    pub replaced: HashSet<String>,
}

impl Replay {
    pub fn from_entries(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut out = Self::default();
        for entry in entries {
            match entry {
                Entry::Result { key, result, agent_id, .. } => {
                    if out.results.contains_key(&key) || out.demoted.contains(&key) || out.failed.contains(&key) {
                        out.replaced.insert(key.clone());
                    }
                    out.demoted.remove(&key);
                    out.results.insert(key, Cached { result, agent_id });
                }
                Entry::Started { key, .. } => {
                    if out.results.remove(&key).is_some() {
                        out.demoted.insert(key.clone());
                    }
                    *out.started.entry(key).or_default() += 1;
                }
                Entry::Failed { key, .. } => {
                    out.results.remove(&key);
                    out.failed.insert(key);
                }
                Entry::Launched | Entry::Resumed => {}
            }
        }
        out
    }

    /// Whether a key with no cached result still keeps the run replaying:
    /// it started before and neither failed nor was demoted (the run was
    /// stopped while that agent worked).
    pub fn was_in_flight(&self, key: &str) -> bool {
        self.started.get(key).is_some_and(|&n| n > 0) && !self.failed.contains(key) && !self.demoted.contains(key)
    }
}

/// The journal file, appended a line at a time.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    /// Whether this process already ended the file with a newline.
    tail_checked: Mutex<bool>,
}

impl Journal {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join(FILE), tail_checked: Mutex::new(false) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Every readable line; a torn last line (a crash mid-write) is skipped.
    pub fn load(&self) -> std::io::Result<Vec<Entry>> {
        let text = std::fs::read_to_string(&self.path)?;
        Ok(text.lines().filter(|l| !l.is_empty() && *l != RESUMED).filter_map(Entry::from_line).collect())
    }

    /// Appends one line. A file whose last line was torn gets a newline and a
    /// `{"type":"resumed"}` fence first, so the torn line stays on its own.
    pub fn append(&self, entry: &Entry) -> std::io::Result<()> {
        let mut checked = self.tail_checked.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(&self.path)?;
        let mut line = String::new();
        if !*checked && torn(&mut file)? {
            line.push('\n');
            line.push_str(RESUMED);
            line.push('\n');
        }
        line.push_str(&entry.to_line());
        line.push('\n');
        file.write_all(line.as_bytes())?;
        *checked = true;
        Ok(())
    }
}

fn torn(file: &mut std::fs::File) -> std::io::Result<bool> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// A call's key (Claude Code's `zr`): `v2:` + SHA-256 over the lane's
/// previous key, the prompt and the key options.
pub fn key(prev: &str, prompt: &str, opts: Option<&Value>) -> String {
    let mut hash = Sha256::new();
    hash.update(prev);
    hash.update([0]);
    hash.update(prompt);
    hash.update([0]);
    hash.update(canonical_options(opts));
    format!("v2:{}", hex::encode(hash.finalize()))
}

/// Where a lane goes on from after a key that ran again (Claude Code's `Ho`):
/// a new result means new keys after it.
pub fn replaced(key: &str, agent_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(key);
    hash.update(b"\0replaced\0");
    hash.update(agent_id);
    hex::encode(hash.finalize())
}

/// One step of a fork (Claude Code's `ro`).
enum Step<'a> {
    Fork,
    Slot(usize),
    Join(&'a [&'a str]),
}

fn fork_hash(prev: &str, kind: &str, step: Step<'_>) -> String {
    let mut hash = Sha256::new();
    hash.update(prev);
    hash.update([0]);
    hash.update(kind);
    hash.update([0]);
    match step {
        Step::Fork => hash.update("fork"),
        Step::Slot(i) => hash.update(format!("slot:{i}")),
        Step::Join(ends) => {
            hash.update("join");
            for end in ends {
                hash.update([0]);
                hash.update(end);
            }
        }
    }
    hex::encode(hash.finalize())
}

/// A run's async lanes (Claude Code's AsyncLocalStorage stores): each holds
/// the last key on it. Lane 0 is the script's own; `parallel()` and
/// `pipeline()` add one per slot.
#[derive(Debug)]
pub struct Lanes {
    prev: Vec<String>,
    forks: Vec<Fork>,
}

impl Default for Lanes {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Fork {
    lane: usize,
    base: String,
    kind: String,
    slots: Vec<usize>,
}

impl Lanes {
    pub fn new() -> Self {
        Self { prev: vec![String::new()], forks: Vec::new() }
    }

    /// The last key on `lane` (an unknown lane is the script's own).
    pub fn prev(&self, lane: usize) -> &str {
        self.prev.get(lane).or_else(|| self.prev.first()).map_or("", String::as_str)
    }

    pub fn set(&mut self, lane: usize, prev: String) {
        let lane = if lane < self.prev.len() { lane } else { 0 };
        if let Some(slot) = self.prev.get_mut(lane) {
            *slot = prev;
        }
    }

    /// `parallel()` / `pipeline()` starting on `lane`: the lane moves on
    /// at once (Claude Code's `Ve`); the slots come later.
    pub fn fork(&mut self, lane: usize, kind: &str) -> usize {
        let lane = if lane < self.prev.len() { lane } else { 0 };
        let base = self.prev(lane).to_owned();
        self.set(lane, fork_hash(&base, kind, Step::Fork));
        self.forks.push(Fork { lane, base, kind: kind.to_owned(), slots: Vec::new() });
        self.forks.len() - 1
    }

    /// A new lane for each of a fork's `n` slots.
    pub fn slots(&mut self, fork: usize, n: usize) -> Vec<usize> {
        let Some(f) = self.forks.get(fork) else { return vec![0; n] };
        let first = self.prev.len();
        let added: Vec<String> = (0..n).map(|i| fork_hash(&f.base, &f.kind, Step::Slot(i))).collect();
        self.prev.extend(added);
        let ids: Vec<usize> = (first..first + n).collect();
        if let Some(f) = self.forks.get_mut(fork) {
            f.slots.clone_from(&ids);
        }
        ids
    }

    /// The fork's lane takes in where each slot ended.
    pub fn join(&mut self, fork: usize) {
        let Some(f) = self.forks.get(fork) else { return };
        let ends: Vec<&str> = f.slots.iter().map(|&s| self.prev(s)).collect();
        let joined = fork_hash(self.prev(f.lane), &f.kind, Step::Join(&ends));
        let lane = f.lane;
        self.set(lane, joined);
    }
}

/// The key options as sorted-key JSON (Claude Code's `ai`).
pub fn canonical_options(opts: Option<&Value>) -> String {
    let Some(Value::Object(opts)) = opts else { return "{}".into() };
    let picked: serde_json::Map<String, Value> =
        KEY_OPTIONS.iter().filter_map(|k| opts.get(*k).map(|v| ((*k).to_owned(), sorted(v)))).collect();
    // serde_json's map keeps keys sorted.
    Value::Object(picked).to_string()
}

fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.iter().filter(|(k, _)| *k != "__proto__").map(|(k, v)| (k.clone(), sorted(v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keys_ignore_label_and_phase_and_chain() {
        let a = key("", "p", Some(&json!({"label": "x", "phase": "P", "model": "haiku"})));
        let b = key("", "p", Some(&json!({"model": "haiku", "label": "y"})));
        assert_eq!(a, b);
        assert_ne!(a, key(&a, "p", Some(&json!({"model": "haiku"}))));
        assert_ne!(a, key("", "p", Some(&json!({"model": "sonnet"}))));
        assert!(a.starts_with("v2:") && a.len() == 67);
        // Claude Code's `zr("A", undefined, "")` (tests/fixtures/cc-lanes.mjs).
        assert!(key("", "A", None).starts_with("v2:737144d25"));
        assert_eq!(
            canonical_options(Some(&json!({"schema": {"b": 1, "a": 2}, "effort": "low"}))),
            r#"{"effort":"low","schema":{"a":2,"b":1}}"#
        );
    }

    #[test]
    fn replay_and_torn_tails() {
        let dir = std::env::temp_dir().join(format!("eos-journal-{}", uuid::Uuid::new_v4()));
        let journal = Journal::new(&dir);
        journal.append(&Entry::Launched).expect("append");
        let (k1, k2) = (key("", "one", None), key("", "two", None));
        journal
            .append(&Entry::Started { key: k1.clone(), agent_id: "a1".into(), label: Some("L".into()), phase: None })
            .expect("append");
        let result = RawValue::from_string(r#"{"z":1,"a":2}"#.into()).expect("raw");
        journal
            .append(&Entry::Result { key: k1.clone(), agent_id: "a1".into(), result, tools_used: None })
            .expect("append");
        journal
            .append(&Entry::Started { key: k2.clone(), agent_id: "a2".into(), label: None, phase: None })
            .expect("append");
        // A crash mid-line.
        std::fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .and_then(|mut f| f.write_all(br#"{"type":"result","key":"v2:"#))
            .expect("tear");
        let replay = Replay::from_entries(journal.load().expect("load"));
        assert_eq!(replay.results.get(&k1).map(|r| r.result.get()), Some(r#"{"z":1,"a":2}"#));
        assert!(replay.was_in_flight(&k2) && !replay.was_in_flight("k3"));

        let again = Journal::new(&dir);
        again.append(&Entry::Failed { key: k2.clone(), agent_id: "a2".into() }).expect("append");
        let text = std::fs::read_to_string(again.path()).expect("read");
        assert!(text.contains("\"key\":\"v2:\n{\"type\":\"resumed\"}\n{\"type\":\"failed\""), "{text}");
        let replay = Replay::from_entries(again.load().expect("load"));
        assert!(replay.failed.contains(&k2) && !replay.was_in_flight(&k2));
        std::fs::remove_dir_all(dir).ok();
    }
}
