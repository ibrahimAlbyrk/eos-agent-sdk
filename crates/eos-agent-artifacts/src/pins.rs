//! Which published files of each artifact a session has seen, and the guard
//! a publish passes before it replaces or removes any of them.
//!
//! A path is pinned when the session reads it (`read` with `path`/`paths`),
//! sees it in a file listing (`list` with `scope: "files"`) or publishes it
//! (a removal pins it as absent). One table per process, by session: a
//! top-level agent and its subagents share one. Before a publish to an
//! existing artifact replaces or removes published paths, the live file list
//! is read and each path is checked:
//!
//! - not pinned: it may be written when nothing is published there, when the
//!   call sends exactly the bytes already there, or when it is named in
//!   `overwrite_unread`; otherwise it is *unread*.
//! - pinned: it may be written when the live file is still what the session
//!   saw (or the call sends the live bytes, or removes a path already gone);
//!   otherwise it *changed*, and `overwrite_unread` does not excuse it.
//!
//! Any unread or changed path refuses the whole publish. A listing that shows
//! a pinned path differing from what the session saw marks it as behind, so
//! a later publish whose listing cannot be read is still refused for it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;

const SESSIONS: usize = 256;
const SLUGS_PER_SESSION: usize = 128;
const PATHS_PER_SLUG: usize = 4096;
/// Problems named one by one in a refusal; the rest are counted.
const NAMED: usize = 20;

/// What the session saw at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seen {
    /// A file with this content hash.
    Sha(String),
    /// Nothing published there.
    Absent,
    /// A file whose hash is not known (a publish rebuilt from a resumed
    /// conversation): only the version it was seen at counts.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    Read,
    List,
    Publish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    seen: Seen,
    ver: String,
    via: Via,
    seq: u64,
    /// A listing showed something else there (what, at which version).
    behind: Option<(Seen, String)>,
}

/// One published path in the live listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveFile {
    pub(crate) path: String,
    /// `None`: the listing gave no hash.
    pub(crate) sha256: Option<String>,
    pub(crate) size: Option<u64>,
    /// Served from the live document (never checked).
    pub(crate) live: bool,
}

/// The live file list of a multi-file artifact, from its boot.
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) ver: String,
    pub(crate) files: Vec<LiveFile>,
}

impl Listing {
    /// The boot's `files`, or `None` for a single page (no separate files).
    pub(crate) fn from_boot(ver: &str, files: Option<&Value>) -> Option<Self> {
        let rows = files?.as_array()?;
        let files: Vec<LiveFile> = rows
            .iter()
            .filter_map(|row| {
                let path = row.as_str().or_else(|| row.get("path").and_then(Value::as_str))?;
                Some(LiveFile {
                    path: path.trim_start_matches('/').to_owned(),
                    sha256: row.get("sha256").and_then(Value::as_str).filter(|s| is_sha(s)).map(str::to_lowercase),
                    size: row.get("size").or_else(|| row.get("sizeBytes")).and_then(Value::as_u64),
                    live: row.get("live").and_then(Value::as_bool) == Some(true),
                })
            })
            .collect();
        (!files.is_empty()).then(|| Self { ver: ver.to_owned(), files })
    }

    pub(crate) fn get(&self, path: &str) -> Option<&LiveFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

pub(crate) fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One path a publish would replace (`sha256`: the bytes it sends, `None`
/// for a copy whose bytes are not known here) or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Touch {
    Write { path: String, sha256: Option<String> },
    Remove(String),
}

impl Touch {
    fn path(&self) -> &str {
        match self {
            Self::Write { path, .. } | Self::Remove(path) => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    Unread { path: String },
    Changed { path: String, seen: Seen, seen_ver: String, via: Via, live: Option<LiveFile> },
}

impl Problem {
    fn path(&self) -> &str {
        match self {
            Self::Unread { path } | Self::Changed { path, .. } => path,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PathPins {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    seq: u64,
    sessions: HashMap<String, Session>,
    order: VecDeque<String>,
}

#[derive(Debug, Default)]
struct Session {
    slugs: HashMap<String, HashMap<String, Pin>>,
    order: VecDeque<String>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn slug(&mut self, session: &str, slug: &str) -> &mut HashMap<String, Pin> {
        if !self.sessions.contains_key(session) {
            self.order.push_back(session.to_owned());
            while self.order.len() > SESSIONS {
                if let Some(old) = self.order.pop_front() {
                    self.sessions.remove(&old);
                }
            }
        }
        let s = self.sessions.entry(session.to_owned()).or_default();
        s.order.retain(|x| x != slug);
        s.order.push_back(slug.to_owned());
        while s.order.len() > SLUGS_PER_SESSION {
            if let Some(old) = s.order.pop_front() {
                s.slugs.remove(&old);
            }
        }
        s.slugs.entry(slug.to_owned()).or_default()
    }

    fn pins(&self, session: &str, slug: &str) -> Option<&HashMap<String, Pin>> {
        self.sessions.get(session)?.slugs.get(slug)
    }
}

/// Sets `pin` unless a newer observation is already there.
fn put(pins: &mut HashMap<String, Pin>, path: &str, pin: Pin) {
    if pins.get(path).is_some_and(|old| old.seq > pin.seq) {
        return;
    }
    if !pins.contains_key(path) && pins.len() >= PATHS_PER_SLUG {
        return;
    }
    pins.insert(path.to_owned(), pin);
}

/// The live file is still what the pin saw. Without hashes on one side,
/// only an unchanged version counts.
fn unchanged(pin: &Pin, live: Option<&LiveFile>, live_ver: &str) -> bool {
    match (&pin.seen, live.map(|f| f.sha256.as_deref())) {
        (Seen::Absent, None) => true,
        (Seen::Sha(a), Some(Some(b))) => a.eq_ignore_ascii_case(b),
        (Seen::Unknown, _) | (Seen::Sha(_), Some(None)) => pin.ver == live_ver,
        (Seen::Absent, Some(_)) | (Seen::Sha(_), None) => false,
    }
}

fn check(
    pins: Option<&HashMap<String, Pin>>,
    listing: &Listing,
    touches: &[Touch],
    acknowledged: &HashSet<String>,
) -> Vec<Problem> {
    let mut out = Vec::new();
    for touch in touches {
        let path = touch.path();
        let live = listing.get(path);
        if live.is_some_and(|f| f.live) {
            continue;
        }
        let sends_live_bytes = matches!(
            (touch, live),
            (Touch::Write { sha256: Some(s), .. }, Some(LiveFile { sha256: Some(l), .. })) if s.eq_ignore_ascii_case(l)
        );
        match pins.and_then(|p| p.get(path)) {
            None => {
                if live.is_none() || acknowledged.contains(path) || sends_live_bytes {
                    continue;
                }
                out.push(Problem::Unread { path: path.to_owned() });
            }
            Some(pin) => {
                if unchanged(pin, live, &listing.ver) || sends_live_bytes {
                    continue;
                }
                if matches!(touch, Touch::Remove(_)) && live.is_none() {
                    continue;
                }
                out.push(Problem::Changed {
                    path: path.to_owned(),
                    seen: pin.seen.clone(),
                    seen_ver: pin.ver.clone(),
                    via: pin.via,
                    live: live.cloned(),
                });
            }
        }
    }
    out
}

impl PathPins {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A number taken before a request: what that request shows does not
    /// overwrite an observation made after it started.
    pub(crate) fn ticket(&self) -> u64 {
        self.lock().next()
    }

    /// The session read `path` at `ver`.
    pub(crate) fn read(&self, session: &str, slug: &str, path: &str, seen: Seen, ver: &str, seq: u64) {
        let mut inner = self.lock();
        let pin = Pin { seen, ver: ver.to_owned(), via: Via::Read, seq, behind: None };
        put(inner.slug(session, slug), path, pin);
    }

    /// The session saw `listing`: unpinned paths (or paths only listed
    /// before) are pinned as listed, and pinned paths it shows differently
    /// are marked behind.
    pub(crate) fn listed(&self, session: &str, slug: &str, listing: &Listing, seq: u64) {
        let mut inner = self.lock();
        let pins = inner.slug(session, slug);
        for file in listing.files.iter().take(PATHS_PER_SLUG) {
            let Some(sha) = &file.sha256 else { continue };
            if pins.get(&file.path).is_none_or(|p| p.via == Via::List && p.seen != Seen::Absent) {
                let pin =
                    Pin { seen: Seen::Sha(sha.clone()), ver: listing.ver.clone(), via: Via::List, seq, behind: None };
                put(pins, &file.path, pin);
            }
        }
        mark_behind(pins, listing, seq);
    }

    /// The session published `written` (with what it sent; `None`: copied on
    /// the server, unknown here, so unpinned) and removed `removed` at `ver`.
    pub(crate) fn published(
        &self,
        session: &str,
        slug: &str,
        ver: &str,
        written: &[(String, Option<Seen>)],
        removed: &[String],
    ) {
        let mut inner = self.lock();
        let seq = inner.next();
        let pins = inner.slug(session, slug);
        for (path, seen) in written {
            match seen {
                None => {
                    pins.remove(path);
                }
                Some(seen) => {
                    let pin = Pin { seen: seen.clone(), ver: ver.to_owned(), via: Via::Publish, seq, behind: None };
                    put(pins, path, pin);
                }
            }
        }
        for path in removed {
            let pin = Pin { seen: Seen::Absent, ver: ver.to_owned(), via: Via::Publish, seq, behind: None };
            put(pins, path, pin);
        }
    }

    pub(crate) fn forget(&self, session: &str, slug: &str) {
        let mut inner = self.lock();
        if let Some(s) = inner.sessions.get_mut(session) {
            s.slugs.remove(slug);
            s.order.retain(|x| x != slug);
        }
    }

    /// Checks a publish against the live `listing` (`None`: it could not be
    /// read; only paths known to be behind are then refused). Returns the
    /// problems and the version they were found at. A refusal for a path
    /// that has since been removed pins it as absent, so publishing it again
    /// creates it afresh.
    pub(crate) fn check(
        &self,
        session: &str,
        slug: &str,
        listing: Option<&Listing>,
        touches: &[Touch],
        acknowledged: &HashSet<String>,
        seq: u64,
    ) -> (Vec<Problem>, String) {
        let mut inner = self.lock();
        let (problems, ver) = match listing {
            Some(listing) => {
                if let Some(pins) = inner.sessions.get_mut(session).and_then(|s| s.slugs.get_mut(slug)) {
                    mark_behind(pins, listing, seq);
                }
                (check(inner.pins(session, slug), listing, touches, acknowledged), listing.ver.clone())
            }
            None => {
                let Some(pins) = inner.pins(session, slug) else { return (Vec::new(), String::new()) };
                let mut remembered = Listing { ver: String::new(), files: Vec::new() };
                let mut behind = Vec::new();
                for touch in touches {
                    let Some((seen, at)) = pins.get(touch.path()).and_then(|p| p.behind.clone()) else { continue };
                    match seen {
                        Seen::Sha(sha) => remembered.files.push(LiveFile {
                            path: touch.path().to_owned(),
                            sha256: Some(sha),
                            size: None,
                            live: false,
                        }),
                        Seen::Unknown => remembered.files.push(LiveFile {
                            path: touch.path().to_owned(),
                            sha256: None,
                            size: None,
                            live: false,
                        }),
                        Seen::Absent => {}
                    }
                    if remembered.ver.is_empty() || at > remembered.ver {
                        remembered.ver = at;
                    }
                    behind.push(touch.clone());
                }
                (check(Some(pins), &remembered, &behind, acknowledged), remembered.ver)
            }
        };
        let gone: Vec<String> = problems
            .iter()
            .filter_map(|p| match p {
                Problem::Changed { path, live: None, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        if !gone.is_empty() {
            let next = inner.next();
            let pins = inner.slug(session, slug);
            for path in gone {
                let pin = Pin { seen: Seen::Absent, ver: ver.clone(), via: Via::List, seq: next, behind: None };
                put(pins, &path, pin);
            }
        }
        (problems, ver)
    }
}

/// Marks pinned paths `listing` shows differently, and clears the mark
/// where it shows them as seen.
fn mark_behind(pins: &mut HashMap<String, Pin>, listing: &Listing, seq: u64) {
    for (path, pin) in pins.iter_mut() {
        if pin.seq > seq {
            continue;
        }
        let live = listing.get(path);
        if live.is_some_and(|f| f.live) || unchanged(pin, live, &listing.ver) {
            pin.behind = None;
        } else {
            let seen = match live {
                None => Seen::Absent,
                Some(LiveFile { sha256: Some(s), .. }) => Seen::Sha(s.clone()),
                Some(_) => Seen::Unknown,
            };
            pin.behind = Some((seen, listing.ver.clone()));
        }
    }
}

/// The refusal the model reads: what changed, what was never read, how to
/// read each one, and when `overwrite_unread` applies.
pub(crate) fn refusal(
    problems: &[Problem],
    live_ver: &str,
    url: &str,
    listing: Option<&Listing>,
    forced: bool,
) -> String {
    const HEAD: &str =
        "Nothing was published or removed: this publish touches files whose published content is not what you last saw";
    let mut changed: Vec<&Problem> = problems.iter().filter(|p| matches!(p, Problem::Changed { .. })).collect();
    // Removed ones first.
    changed.sort_by_key(|p| !matches!(p, Problem::Changed { live: None, .. }));
    let unread: Vec<&Problem> = problems.iter().filter(|p| matches!(p, Problem::Unread { .. })).collect();
    let now = |path: &str| -> String {
        match listing.and_then(|l| l.get(path)) {
            None => "  now: deleted; publishing this path again creates it afresh, so do that only if the user wants it back"
                .to_owned(),
            Some(f) => {
                let mut what: Vec<String> = Vec::new();
                what.extend(f.sha256.as_ref().map(|s| format!("sha256 {s}")));
                what.extend(f.size.map(|n| format!("{n} bytes")));
                let what = if what.is_empty() { "published".to_owned() } else { what.join(", ") };
                format!("  now: {what}; read it with action \"read\", url {url}, path {path:?}")
            }
        }
    };
    let rest = |n: usize, shown: usize| {
        let lead = if shown > 0 {
            format!("and {} more", n - shown)
        } else {
            format!("{n} path{}, not named here", if n == 1 { "" } else { "s" })
        };
        format!("- {lead}: action \"list\" with scope \"files\" and url {url} lists every file with its size")
    };
    let mut parts = vec![if changed.is_empty() {
        format!("{HEAD}, and sending your copy could discard content you have not read.")
    } else {
        format!("{HEAD}, and sending your copy would replace a newer edit.")
    }];
    let shown_changed = changed.len().min(NAMED);
    if !changed.is_empty() {
        let mut lines: Vec<String> = changed[..shown_changed]
            .iter()
            .filter_map(|p| {
                let Problem::Changed { path, seen, seen_ver, via, live } = p else { return None };
                let then = match (seen, via) {
                    (Seen::Absent, Via::Publish) => format!("you removed it at version {seen_ver}"),
                    (Seen::Absent, _) => format!("it was gone when you last checked (version {seen_ver})"),
                    _ => format!("you last saw it at version {seen_ver}"),
                };
                let since = match (live, seen) {
                    (None, _) => format!("it has since been removed (version {live_ver})"),
                    (Some(_), Seen::Absent) => format!("someone has since published a file there (version {live_ver})"),
                    _ => format!("someone has since replaced it (version {live_ver})"),
                };
                Some(format!("- {path:?}: {then}; {since}\n{}", now(path)))
            })
            .collect();
        if changed.len() > shown_changed {
            lines.push(rest(changed.len(), shown_changed));
        }
        let deleted = changed.iter().any(|p| matches!(p, Problem::Changed { live: None, .. }));
        parts.push(format!(
            "Changed since you last saw them:\n{}\nRead each one{} again (action \"read\" with its `path`), re-apply your change to what it holds now, and publish again{}. Never resend your earlier copy.{}",
            lines.join("\n"),
            if deleted { " that still exists" } else { "" },
            if deleted { "; a deleted one needs no read" } else { "" },
            if forced { " `force` does not override this." } else { "" },
        ));
    }
    if !unread.is_empty() {
        let shown = unread.len().min(NAMED - shown_changed);
        let mut lines: Vec<String> =
            unread[..shown].iter().map(|p| format!("- {:?}\n{}", p.path(), now(p.path()))).collect();
        if unread.len() > shown {
            lines.push(rest(unread.len(), shown));
        }
        parts.push(format!(
            "Already published, and not read as published or listed by you in this session:\n{}\nRead each one first (action \"read\" with its `path`) and build on it. Only when the user asked for the file to be replaced without looking, name it in `overwrite_unread`.",
            lines.join("\n")
        ));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn listing(ver: &str, files: &[(&str, &str)]) -> Listing {
        Listing {
            ver: ver.into(),
            files: files
                .iter()
                .map(|(p, s)| LiveFile { path: (*p).into(), sha256: Some((*s).into()), size: None, live: false })
                .collect(),
        }
    }

    fn write(path: &str, sha: &str) -> Touch {
        Touch::Write { path: path.into(), sha256: Some(sha.into()) }
    }

    #[test]
    fn unread_changed_and_acknowledged() {
        let pins = PathPins::default();
        let none = HashSet::new();
        let live = listing("v2", &[("data.json", A)]);
        // Never seen and present: unread.
        let (p, _) = pins.check("s", "x", Some(&live), &[write("data.json", B)], &none, pins.ticket());
        assert_eq!(p, vec![Problem::Unread { path: "data.json".into() }]);
        // Acknowledged: passes. New paths always pass.
        let ack: HashSet<String> = ["data.json".to_owned()].into();
        assert!(pins.check("s", "x", Some(&live), &[write("data.json", B)], &ack, pins.ticket()).0.is_empty());
        assert!(pins.check("s", "x", Some(&live), &[write("new.json", B)], &none, pins.ticket()).0.is_empty());
        // Sending the live bytes passes.
        assert!(pins.check("s", "x", Some(&live), &[write("data.json", A)], &none, pins.ticket()).0.is_empty());

        // Read at v1 with A, still A at v2: passes. Another session: unread.
        pins.read("s", "x", "data.json", Seen::Sha(A.into()), "v1", pins.ticket());
        assert!(pins.check("s", "x", Some(&live), &[write("data.json", B)], &none, pins.ticket()).0.is_empty());
        assert_eq!(pins.check("t", "x", Some(&live), &[write("data.json", B)], &none, pins.ticket()).0.len(), 1);

        // Replaced since: changed, and acknowledging does not excuse it.
        let newer = listing("v3", &[("data.json", B)]);
        let (p, ver) = pins.check("s", "x", Some(&newer), &[write("data.json", A)], &ack, pins.ticket());
        assert!(matches!(&p[..], [Problem::Changed { .. }]) && ver == "v3", "{p:?}");
        // Without a listing, the remembered difference still refuses it.
        assert_eq!(pins.check("s", "x", None, &[write("data.json", A)], &ack, pins.ticket()).0.len(), 1);

        // Its own publish pins what it sent.
        pins.published("s", "x", "v4", &[("data.json".into(), Some(Seen::Sha(A.into())))], &[]);
        let after = listing("v4", &[("data.json", A)]);
        assert!(pins.check("s", "x", Some(&after), &[write("data.json", B)], &none, pins.ticket()).0.is_empty());
    }

    #[test]
    fn a_removed_file_can_be_created_afresh_after_the_refusal() {
        let pins = PathPins::default();
        let none = HashSet::new();
        pins.read("s", "x", "a.css", Seen::Sha(A.into()), "v1", pins.ticket());
        let gone = listing("v2", &[("index.html", B)]);
        let (p, _) = pins.check("s", "x", Some(&gone), &[write("a.css", B)], &none, pins.ticket());
        assert_eq!(p.len(), 1);
        let text = refusal(&p, "v2", "https://u", Some(&gone), false);
        assert!(text.contains("it has since been removed (version v2)"), "{text}");
        assert!(pins.check("s", "x", Some(&gone), &[write("a.css", B)], &none, pins.ticket()).0.is_empty());
    }

    #[test]
    fn an_older_listing_never_overwrites_a_newer_read() {
        let pins = PathPins::default();
        let none = HashSet::new();
        let early = pins.ticket();
        pins.read("s", "x", "a.js", Seen::Sha(B.into()), "v2", pins.ticket());
        pins.listed("s", "x", &listing("v1", &[("a.js", A), ("b.js", A)]), early);
        let live = listing("v2", &[("a.js", B), ("b.js", A)]);
        assert!(
            pins.check("s", "x", Some(&live), &[write("a.js", A), write("b.js", B)], &none, pins.ticket()).0.is_empty()
        );
    }
}
