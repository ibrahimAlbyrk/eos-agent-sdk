//! A `comments/<slug>` answer read as Claude Code reads it (`yt`): each
//! thread's comments with their lanes, sends to Claude and authors' access,
//! and its anchor, checked field by field (a malformed field is dropped).

use serde_json::Value;

use super::comments::{Stamp, stamp};
use super::frame::{Anchor, Lane, Row, clean, invisible};

/// One thread as read.
#[derive(Debug, Clone)]
pub(super) struct Thread {
    pub(super) id: String,
    pub(super) resolved: bool,
    pub(super) resolved_at: Option<String>,
    /// A writer activated Claude here with edit capability (`claude_capability: "edit"`).
    pub(super) edit_capable: bool,
    pub(super) activated_raw: Option<String>,
    pub(super) activated_by: Option<String>,
    pub(super) anchor: Anchor,
    pub(super) rows: Vec<Row>,
    /// Each row's send to Claude, as sent.
    pub(super) sent: Vec<Option<String>>,
    pub(super) created: Vec<Option<String>>,
}

impl Thread {
    pub(super) fn activated(&self) -> Option<Stamp> {
        self.activated_raw.as_deref().and_then(stamp)
    }
}

/// Claude Code's `K5e`: ids of comments.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Claude Code's `Elt`: a time as the server writes it.
fn valid_time(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_digit() || b"T:.-+Z ".contains(&b))
}

/// Claude Code's `ot`: a CSS path of at most 10 steps — `#id` or
/// `[data-id="…"]` first, then `tag:nth-of-type(n)`.
fn valid_path(path: &str) -> bool {
    let steps: Vec<&str> = path.split(" > ").collect();
    path.len() <= 1024
        && steps.len() <= 10
        && steps.iter().enumerate().all(|(i, s)| nth_of_type(s) || (i == 0 && (hash_id(s) || data_id(s))))
}

fn hash_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('#') else { return false };
    let b = rest.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_alphabetic() || b[0] == b'_' || b[0] == b'-')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

fn data_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("[data-id=\"").and_then(|r| r.strip_suffix("\"]")) else { return false };
    !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn nth_of_type(s: &str) -> bool {
    let Some((tag, n)) = s.split_once(":nth-of-type(") else { return false };
    let Some(n) = n.strip_suffix(')') else { return false };
    let t = tag.as_bytes();
    !t.is_empty()
        && t.len() <= 24
        && t[0].is_ascii_lowercase()
        && t.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !n.is_empty()
        && n.len() <= 4
        && !n.starts_with('0')
        && n.bytes().all(|c| c.is_ascii_digit())
}

/// Claude Code's `ce`: a short anchor text, cleaned and collapsed.
fn short_text(v: &Value, max_bytes: usize) -> Option<String> {
    let s = v.as_str().filter(|s| !s.is_empty() && s.len() <= max_bytes)?;
    let s: String = clean(s).chars().map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// Claude Code's `lt`: the page (file) a thread is on, or that it could not be read.
fn anchor_file(v: &Value) -> (Option<String>, bool) {
    match v {
        Value::Null => (None, false),
        Value::String(s) if s.is_empty() => (None, false),
        Value::String(f) => {
            let ok = f.len() <= 512
                && !f
                    .chars()
                    .any(|c| (c as u32) < 32 || c == '\u{7f}' || matches!(c, '%' | ';' | '\\') || invisible(c))
                && !f.ends_with(".cmekshadow")
                && !f.starts_with('/')
                && f.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
                && !f.starts_with('_');
            if ok { (Some(f.clone()), false) } else { (None, true) }
        }
        _ => (None, true),
    }
}

/// Claude Code's `ht`: a drawn region (and the children it covers) on a path anchor.
fn region(v: &Value) -> Option<Vec<usize>> {
    let unit = |k: &str| v[k].as_f64().filter(|x| (0.0..=1.0).contains(x)).map(|x| (x * 1000.0).round() / 1000.0);
    let (x0, y0, x1, y1) = (unit("x0")?, unit("y0")?, unit("x1")?, unit("y1")?);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let kids: Vec<usize> = match &v["kids"] {
        Value::Array(kids) => {
            let k: Vec<usize> = kids.iter().filter_map(|k| k.as_u64().map(|n| n as usize)).collect();
            let ok = k.len() == kids.len()
                && k.len() <= 8
                && k.iter().all(|n| *n < 4096)
                && k.windows(2).all(|w| w[0] < w[1]);
            if ok { k } else { Vec::new() }
        }
        _ => Vec::new(),
    };
    Some(kids)
}

fn anchor(t: &Value) -> Anchor {
    let a = &t["anchor"];
    let path = a["path"].as_str().filter(|p| !p.is_empty() && valid_path(p)).map(str::to_owned);
    let span_quote = a["span"]["quote"]
        .as_str()
        .filter(|q| !q.is_empty() && q.len() <= 512 && a["span"]["start"].as_u64().is_some_and(|s| s <= 16_777_216))
        .map(clean)
        .filter(|q| !q.trim().is_empty());
    let label = short_text(&a["label"], 128);
    let detail = short_text(&a["detail"], 512);
    let path_unreadable =
        path.is_some() && a["path_unreadable"] == Value::Bool(true) && (label.is_some() || detail.is_some());
    let (file, file_degraded) = anchor_file(&a["file"]);
    let kids = if path.is_some() && a["span"].is_null() { region(&a["region"]) } else { None };
    Anchor {
        span_quote,
        path_unreadable,
        file,
        file_degraded,
        label,
        detail,
        region: kids.is_some(),
        region_kids: kids.unwrap_or_default(),
        moved_at: a["moved_at"].as_str().filter(|s| valid_time(s)).map(str::to_owned),
        moved_comment_id: a["moved_comment_id"].as_str().filter(|s| valid_id(s)).map(str::to_owned),
        carried: t["carried"] == Value::Bool(true),
        path,
    }
}

/// Claude Code's `Py`: no role is a person, `degraded` unreadable, any other
/// an agent (`human`, which the API does not send for people, is read as one).
fn lane(role: &Value) -> Lane {
    match role.as_str() {
        None | Some("" | "human") => Lane::Human,
        Some("degraded") => Lane::Unknown,
        Some(r) if r.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') && r.len() <= 64 => Lane::Agent,
        Some(_) => Lane::Unknown,
    }
}

/// The threads of an answer.
pub(super) fn threads(body: &Value) -> Vec<Thread> {
    let mut out: Vec<Thread> = Vec::new();
    for t in body["threads"].as_array().into_iter().flatten() {
        let Some(id) = t["id"].as_str() else { continue };
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        let mut thread = Thread {
            id: id.to_owned(),
            resolved: t["resolved_at"].as_str().is_some_and(|s| !s.is_empty()),
            resolved_at: t["resolved_at"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
            edit_capable: t["claude_capability"] == "edit",
            activated_raw: t["claude_activated_at"].as_str().filter(|s| stamp(s).is_some()).map(str::to_owned),
            activated_by: t["claude_activated_by"].as_str().filter(|s| !s.is_empty()).map(str::to_ascii_lowercase),
            anchor: anchor(t),
            rows: Vec::new(),
            sent: Vec::new(),
            created: Vec::new(),
        };
        for c in t["comments"].as_array().into_iter().flatten() {
            let (Some(cid), Some(text)) = (c["id"].as_str(), c["text"].as_str()) else { continue };
            if thread.rows.iter().any(|r| r.id == cid) {
                continue;
            }
            let lane = lane(&c["author"]["role"]);
            let account = c["author"]["account"].as_str().unwrap_or("unknown").to_ascii_lowercase();
            let access = c["author"]["access"]
                .as_str()
                .filter(|a| lane == Lane::Human && matches!(*a, "owner" | "editor" | "commenter"))
                .map(str::to_owned);
            // Claude Code's `Zkr`: a source other than an agent's own says
            // the artifact's page posted it.
            let source = c["source"].as_str().unwrap_or_default();
            let by_artifact = !source.is_empty() && !(source == "agent" && c["author"]["role"] == "assistant");
            thread.rows.push(Row {
                id: cid.to_owned(),
                account,
                lane,
                text: clean(text),
                access,
                by_artifact,
                sent_by_viewer: c["sent_by_viewer"].as_bool(),
            });
            thread.sent.push(c["to_claude_at"].as_str().filter(|s| stamp(s).is_some()).map(str::to_owned));
            thread.created.push(c["created_at"].as_str().filter(|s| valid_time(s)).map(str::to_owned));
        }
        out.push(thread);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_are_read_field_by_field() {
        let body = serde_json::json!({ "threads": [
            { "id": "t1", "claude_capability": "edit", "claude_activated_at": "2026-11-08T08:00:00Z",
              "anchor": { "path": "#hero > p:nth-of-type(2)", "label": " Pricing\n plans ", "file": "docs/a.html",
                          "region": { "x0": 0.1, "y0": 0.1, "x1": 0.5, "y1": 0.4, "kids": [0, 2] },
                          "moved_comment_id": "c1" },
              "comments": [
                { "id": "c1", "author": { "account": "U_1", "access": "editor" }, "text": "hi\r\nthere",
                  "to_claude_at": "2026-11-08T08:00:01Z", "sent_by_viewer": true },
                { "id": "c1", "author": { "account": "u_2" }, "text": "duplicate" },
                { "id": "c2", "author": { "account": "u_2", "role": "assistant" }, "text": "Done",
                  "source": "agent" },
                { "id": "c3", "author": { "account": "u_3" }, "text": "from the page", "source": "page" },
                { "id": "c4", "author": { "account": "u_3", "role": "degraded" }, "text": "?" }
            ]},
            { "id": "t2", "anchor": { "path": "body > script", "file": "/etc/passwd" }, "comments": [] }
        ]});
        let t = threads(&body);
        assert_eq!(t.len(), 2);
        let a = &t[0];
        assert!(a.activated().is_some() && a.edit_capable && !t[1].edit_capable);
        assert_eq!(a.anchor.path.as_deref(), Some("#hero > p:nth-of-type(2)"));
        assert_eq!(a.anchor.label.as_deref(), Some("Pricing plans"));
        assert_eq!(
            (a.anchor.file.as_deref(), a.anchor.region, &a.anchor.region_kids),
            (Some("docs/a.html"), true, &vec![0, 2])
        );
        assert_eq!(a.rows.len(), 4, "a repeated comment id is read once");
        assert_eq!((a.rows[0].text.as_str(), a.rows[0].account.as_str()), ("hi\nthere", "u_1"));
        assert_eq!((a.rows[0].access.as_deref(), a.rows[0].sent_by_viewer), (Some("editor"), Some(true)));
        assert_eq!((a.rows[1].lane, a.rows[1].by_artifact), (Lane::Agent, false));
        assert!(a.rows[2].by_artifact && a.rows[2].lane == Lane::Human);
        assert_eq!(a.rows[3].lane, Lane::Unknown);
        assert_eq!(a.sent[0].as_deref(), Some("2026-11-08T08:00:01Z"));
        let b = &t[1];
        assert!(b.anchor.path.is_none() && b.anchor.file_degraded, "a bad path and a bad file are dropped");
    }
}
