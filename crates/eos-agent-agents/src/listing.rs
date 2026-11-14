//! Claude Code's ListAgents rows for subagents and the `[ref]` that tells
//! two agents of one name apart.

use std::collections::HashMap;

use eos_agent_core::SubagentInfo;
use sha2::{Digest, Sha256};

/// Claude Code's shortest and longest ref.
const MIN_REF: usize = 6;
/// Rows shown before "(… N more not shown)".
const MAX_ROWS: usize = 100;

/// Claude Code's `N(kind, id)`: the first 12 hex digits of
/// `sha256("<kind>:<id>")`.
fn full_ref(kind: &str, id: &str) -> String {
    let digest = Sha256::digest(format!("{kind}:{id}").as_bytes());
    hex::encode(&digest[..6])
}

/// Each named subagent's ref (by agent id): as long as it takes to differ
/// from its neighbours among every addressable entry (the main conversation
/// and the named subagents), at least six digits.
pub(crate) fn refs(session: Option<&str>, agents: &[&SubagentInfo]) -> HashMap<String, String> {
    let named: Vec<(&str, String)> = agents
        .iter()
        .filter(|a| a.name.is_some())
        .map(|a| (a.agent_id.as_str(), full_ref("subagent", &a.agent_id)))
        .collect();
    let mut all: Vec<&str> = named.iter().map(|(_, r)| r.as_str()).collect();
    let main = session.map(|s| full_ref("main", s));
    all.extend(main.as_deref());
    all.sort_unstable();
    all.dedup();
    let common = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let length = |r: &str| {
        let at = all.binary_search(&r).unwrap_or(0);
        let before = at.checked_sub(1).map_or(0, |i| common(r, all[i]));
        let after = all.get(at + 1).map_or(0, |n| common(r, n));
        r.len().min(MIN_REF.max(before.max(after) + 1))
    };
    named.iter().map(|(id, r)| ((*id).to_owned(), r[..length(r)].to_owned())).collect()
}

/// Claude Code's "Subagents" section: `name [ref]` (or the id when it has no
/// name), type, status, how long ago it started.
pub(crate) fn listing(session: Option<&str>, agents: &[SubagentInfo], now_ms: u64) -> String {
    let agents: Vec<&SubagentInfo> = agents.iter().filter(|a| !a.workflow && !a.delisted).collect();
    if agents.is_empty() {
        return "No reachable agents.".to_owned();
    }
    let refs = refs(session, &agents);
    let mut out = format!("Subagents ({}):", agents.len());
    for agent in agents.iter().take(MAX_ROWS) {
        let printed = match (&agent.name, refs.get(&agent.agent_id)) {
            (Some(name), Some(r)) => format!("{name} [{r}]"),
            _ => agent.agent_id.clone(),
        };
        let ago = most_significant(now_ms.saturating_sub(agent.started_ms));
        out += &format!("\n  {printed}  ·  {}  ·  {}  ·  started {ago} ago", agent.agent_type, agent.status);
    }
    if agents.len() > MAX_ROWS {
        out += &format!("\n  (\u{2026} {} more not shown)", agents.len() - MAX_ROWS);
    }
    out
}

/// Claude Code's `cn(ms, {mostSignificantOnly: true})`.
fn most_significant(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{}s", ms / 1000);
    }
    let mut days = ms / 86_400_000;
    let mut hours = ms % 86_400_000 / 3_600_000;
    let mut minutes = ms % 3_600_000 / 60_000;
    let seconds = (ms % 60_000 + 500) / 1000;
    if seconds == 60 {
        minutes += 1;
    }
    if minutes == 60 {
        minutes = 0;
        hours += 1;
    }
    if hours == 24 {
        hours = 0;
        days += 1;
    }
    match (days, hours, minutes) {
        (d, _, _) if d > 0 => format!("{d}d"),
        (_, h, _) if h > 0 => format!("{h}h"),
        (_, _, m) if m > 0 => format!("{m}m"),
        _ => format!("{}s", if seconds == 60 { 0 } else { seconds }),
    }
}

/// Claude Code's `O7`: `name [ref]` split into its parts.
pub(crate) fn split_ref(to: &str) -> Option<(&str, &str)> {
    let to = to.trim();
    let open = to.strip_suffix(']')?.rfind('[')?;
    let r = &to[open + 1..to.len() - 1];
    let name = to[..open].trim_end();
    let ok = (MIN_REF..=12).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (ok && !name.is_empty()).then_some((name, r))
}

/// The listed agent `name [ref]` points at, by its full ref.
pub(crate) fn by_ref<'a>(agents: &'a [SubagentInfo], name: &str, r: &str) -> Option<&'a SubagentInfo> {
    agents
        .iter()
        .find(|a| !a.delisted && a.name.as_deref() == Some(name) && full_ref("subagent", &a.agent_id).starts_with(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_as_claude_code_prints_them() {
        assert_eq!(most_significant(4_200), "4s");
        assert_eq!(most_significant(59_999), "59s");
        assert_eq!(most_significant(90_000), "1m");
        assert_eq!(most_significant(3_599_700), "1h");
        assert_eq!(most_significant(2 * 86_400_000 + 5), "2d");
    }

    #[test]
    fn refs_split_and_match() {
        assert_eq!(split_ref("scout [a1b2c3]"), Some(("scout", "a1b2c3")));
        assert_eq!(split_ref("scout[a1b2c3d4]"), Some(("scout", "a1b2c3d4")));
        assert_eq!(split_ref("scout [xyz123]"), None);
        assert_eq!(split_ref("scout [a1b]"), None);
        assert_eq!(full_ref("subagent", "a1").len(), 12);
    }
}
