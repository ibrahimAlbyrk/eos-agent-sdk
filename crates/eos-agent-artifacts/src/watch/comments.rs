//! Comments sent to Claude: which threads have news, read in one go per
//! burst of the live channel's comment notes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use std::collections::HashSet;

use serde_json::Value;

use super::coalesce::{artifact_name, notification};
use super::frame::{Lane, Trigger};
use super::replies::{Fresh, Reaction, Summary};
use super::{Inner, ack, notices, thread};

impl Inner {
    /// Reads the threads; for each one with comments sent to Claude since
    /// last time (or newly activated), the agent that answers this
    /// artifact's comments gets it handed over or answered (Claude Code's
    /// auto-replies), and is told.
    pub(super) async fn comments(self: &Arc<Self>, slug: &str) {
        let Ok(body) = self.client_for(slug).comments(slug).await else { return };
        let threads = thread::threads(&body);
        let viewer = viewer_of(&body);
        let newest = threads
            .iter()
            .flat_map(|t| {
                let sends = t.rows.iter().zip(&t.sent).filter(|(r, _)| r.lane == Lane::Human);
                sends.filter_map(|(_, at)| at.as_deref().and_then(stamp)).chain(t.activated())
            })
            .max();
        let now = now_ms();
        let (owner, url, title, fresh) = {
            let mut watches = self.lock();
            let Some(watch) = watches.get_mut(slug) else { return };
            let (primed, after) = (watch.comments_primed, watch.comments_after.clone());
            watch.comments_primed = true;
            if newest > after {
                watch.comments_after = newest;
            }
            let seen = std::mem::take(&mut watch.seen);
            watch.seen =
                threads.iter().map(|t| (t.id.clone(), t.rows.iter().map(|r| r.id.clone()).collect())).collect();
            // Claude Code's first read only learns what is there, except that
            // an explicit watch (or a resumed session's) picks up comments
            // sent to Claude in the last minutes (`pickUpRecentSummons`).
            let pick_up = std::mem::take(&mut watch.pick_up_recent);
            if !primed && !pick_up {
                return;
            }
            let Some(owner) = watch.subscribers.iter().filter(|s| s.replies).max_by_key(|s| s.since) else {
                return;
            };
            let viewer = viewer.as_deref();
            let fresh: Vec<Fresh> = threads
                .into_iter()
                .filter_map(|t| match (primed, seen.get(&t.id)) {
                    (false, _) => picked_up(t, now, viewer),
                    (true, None) => first_sight(t, now, viewer),
                    (true, Some(seen)) => news(t, after.as_ref(), seen, viewer),
                })
                .collect();
            (Arc::clone(&owner.agent), watch.url.clone(), watch.title.clone(), fresh)
        };
        let name = artifact_name(title.as_deref(), &url);
        let names = &body["names"];
        let mut notes = Vec::new();
        for fresh in fresh {
            let note = |summary: Summary, detail: &str| notification(&summary.text(&name), detail);
            for reaction in self.answer(slug, &url, &owner, &fresh).await {
                match reaction {
                    Reaction::Quiet => {}
                    Reaction::Note(summary, detail) => notes.push((Arc::clone(&owner), note(summary, &detail), true)),
                    Reaction::Gathered(family, detail) => {
                        self.coalesce(slug, &owner, family, name.clone(), &fresh.thread.id, detail);
                    }
                    Reaction::Unanswered => {
                        let comments: Vec<notices::Comment> = fresh
                            .thread
                            .rows
                            .iter()
                            .filter(|r| fresh.summons.contains(&r.id))
                            .map(|r| notices::Comment {
                                author: names[&r.account]
                                    .as_str()
                                    .filter(|n| !n.is_empty())
                                    .unwrap_or("someone")
                                    .to_owned(),
                                text: r.text.clone(),
                            })
                            .collect();
                        let detail = notices::comments_waiting(&url, &fresh.thread.id, &comments);
                        notes.push((Arc::clone(&owner), note(Summary::Waiting, &detail), true));
                    }
                }
            }
        }
        self.deliver(slug, notes).await;
    }
}

/// Claude Code's `Yi`: a comment sent to Claude this close to now counts
/// as recent (`Vn`).
const RECENT_MS: i64 = 600_000;

/// What is new on a thread seen before, since `after`: comments sent to
/// Claude since then (each new to this watch or sent again), or the
/// thread's activation. Claude Code acts only on activated threads.
fn news(t: thread::Thread, after: Option<&Stamp>, seen: &HashSet<String>, viewer: Option<&str>) -> Option<Fresh> {
    t.activated_raw.as_ref()?;
    let later = |at: &Option<String>| at.as_deref().and_then(stamp).is_some_and(|s| Some(&s) > after);
    let summons: Vec<usize> =
        (0..t.rows.len()).filter(|i| t.rows[*i].lane == Lane::Human && later(&t.sent[*i])).collect();
    let activated = later(&t.activated_raw);
    if summons.is_empty() && !(activated && !t.resolved) {
        return None;
    }
    Some(fresh(t, summons, Some(seen), viewer))
}

/// Claude Code's first sight of a thread that appeared after the first
/// read (`Bs` with `actOnFirstSight`): an activated, open thread with a
/// comment not by an agent is taken up as an activation (with the fast
/// acknowledgement on, its recent comments sent to Claude count as new);
/// any other is only learnt.
fn first_sight(t: thread::Thread, now: i64, viewer: Option<&str>) -> Option<Fresh> {
    let ready = t.activated_raw.is_some() && !t.resolved && t.rows.iter().any(|r| r.lane != Lane::Agent);
    if !ready {
        return None;
    }
    let summons = if ack::enabled() { recent_summons(&t, now) } else { Vec::new() };
    Some(fresh(t, summons, None, viewer))
}

/// The first read of an explicit or resumed watch: comments sent to Claude
/// in the last ten minutes on activated threads are taken up as new.
fn picked_up(t: thread::Thread, now: i64, viewer: Option<&str>) -> Option<Fresh> {
    t.activated_raw.as_ref()?;
    let summons = recent_summons(&t, now);
    if summons.is_empty() {
        return None;
    }
    Some(fresh(t, summons, None, viewer))
}

/// Claude Code's `OBe` filtered by `Vn`: the people's comments sent to
/// Claude that no agent answered yet (sent after the last agent comment,
/// and after the thread was resolved), sent within ten minutes of `now`.
fn recent_summons(t: &thread::Thread, now: i64) -> Vec<usize> {
    if t.rows.iter().any(|r| r.lane == Lane::Unknown) {
        return Vec::new();
    }
    let sent_ms = |i: usize| t.sent[i].as_deref().and_then(millis);
    let last_agent = t.rows.iter().rposition(|r| r.lane == Lane::Agent);
    let later_than = |i: usize, at: Option<&str>| match (sent_ms(i), at.and_then(millis)) {
        (Some(sent), Some(at)) => sent > at,
        _ => false,
    };
    (0..t.rows.len())
        .filter(|&i| t.rows[i].lane == Lane::Human && sent_ms(i).is_some())
        .filter(|&i| last_agent.is_none_or(|a| i > a || later_than(i, t.created[a].as_deref())))
        .filter(|&i| !t.resolved || later_than(i, t.resolved_at.as_deref()))
        .filter(|&i| sent_ms(i).is_some_and(|sent| (now - sent).abs() <= RECENT_MS))
        .collect()
}

/// The thread's news: the comments in `summons` (none: its activation).
fn fresh(t: thread::Thread, summons: Vec<usize>, seen: Option<&HashSet<String>>, viewer: Option<&str>) -> Fresh {
    let was_seen = |i: &usize| seen.is_some_and(|s| s.contains(&t.rows[*i].id));
    let ids: Vec<String> = summons.iter().map(|i| t.rows[*i].id.clone()).collect();
    let trigger = if summons.is_empty() {
        Trigger::Activation
    } else if t.anchor.moved_comment_id.as_ref().is_some_and(|m| ids.iter().all(|id| id == m)) {
        Trigger::Moved
    } else if summons.iter().any(|i| !was_seen(i)) {
        Trigger::Fresh
    } else {
        Trigger::Redesignated
    };
    let gesture_at = if summons.is_empty() {
        t.activated_raw.clone()
    } else {
        newest_raw(summons.iter().filter_map(|i| t.sent[*i].as_deref()))
    };
    // Claude Code's `un`: the user's own send (or activation) is not work it
    // took up unprompted.
    let own = if summons.is_empty() {
        viewer.is_some() && t.activated_by.as_deref() == viewer
    } else {
        summons.iter().all(|i| t.rows[*i].sent_by_viewer == Some(true) && !t.rows[*i].by_artifact)
    };
    let unseen_human =
        (0..t.rows.len()).any(|i| !was_seen(&i) && t.rows[i].lane == Lane::Human && !t.rows[i].by_artifact);
    let fresh_human = unseen_human || trigger == Trigger::Redesignated;
    Fresh { thread: t, trigger, summons: ids, gesture_at, own, fresh_human, viewer: viewer.map(str::to_owned) }
}

/// Milliseconds since the epoch of an RFC 3339 UTC time.
pub(super) fn millis(s: &str) -> Option<i64> {
    let s = s.trim();
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once(['T', ' '])?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms = format!("{frac:0<3}")[..3].parse::<i64>().ok()?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + min) * 60_000 + sec * 1000 + ms)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// An RFC 3339 UTC time, comparable: digits of the date and time, then the
/// fraction padded to nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Stamp(String);

/// Claude Code's `vqo`: of these times, the newest well-formed one, as sent.
pub(super) fn newest_raw<'a>(times: impl IntoIterator<Item = &'a str>) -> Option<String> {
    times.into_iter().filter_map(|t| Some((stamp(t)?, t))).max_by(|a, b| a.0.cmp(&b.0)).map(|(_, t)| t.to_owned())
}

pub(super) fn stamp(s: &str) -> Option<Stamp> {
    let s = s.trim().trim_end_matches('Z').trim_end_matches("+00:00");
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    let digits: String = whole.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 14 || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(Stamp(format!("{digits}{fraction:0<9}")))
}

/// The reading account, as the answer names it (`viewer`).
fn viewer_of(body: &Value) -> Option<String> {
    let v = &body["viewer"];
    v.as_str().or_else(|| v["account"].as_str()).or_else(|| v["id"].as_str()).map(str::to_ascii_lowercase)
}

/// Comment notes come in bursts: one read of the threads per burst.
pub(super) fn schedule_comment_check(inner: &Arc<Inner>, slug: &str) {
    let Some(flag) = inner.lock().get(slug).map(|w| Arc::clone(&w.comment_check)) else { return };
    if flag.swap(true, Ordering::SeqCst) {
        return;
    }
    let (weak, slug, wait) = (Arc::downgrade(inner), slug.to_owned(), inner.config.comment_debounce);
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        flag.store(false, Ordering::SeqCst);
        if let Some(inner) = weak.upgrade() {
            inner.comments(&slug).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_order() {
        let a = stamp("2026-11-08T07:52:52Z").expect("stamp");
        let b = stamp("2026-11-08T07:52:52.545159Z").expect("stamp");
        assert!(b > a);
        assert!(stamp("yesterday").is_none());
    }

    #[test]
    fn news_names_what_brought_claude() {
        let body = serde_json::json!({ "threads": [
            { "id": "t1", "claude_activated_at": "2026-11-08T07:00:00Z", "anchor": { "moved_comment_id": "c2" },
              "comments": [
                { "id": "c1", "author": { "account": "u_1" }, "text": "old", "to_claude_at": "2026-11-08T08:00:00Z" },
                { "id": "c2", "author": { "account": "u_1" }, "text": "moved", "to_claude_at": "2026-11-08T09:00:00Z",
                  "sent_by_viewer": true }
            ]},
            { "id": "t2", "claude_activated_at": "2026-11-08T09:00:00Z", "claude_activated_by": "V1", "comments": [
                { "id": "c3", "author": { "account": "u_2" }, "text": "waiting" }
            ]},
            { "id": "t3", "comments": [
                { "id": "c4", "author": { "account": "u_2" }, "text": "not activated", "to_claude_at": "2026-11-08T09:00:00Z" }
            ]}
        ]});
        let after = stamp("2026-11-08T08:30:00Z");
        let mut t = thread::threads(&body).into_iter();
        let seen: HashSet<String> = ["c1".to_owned()].into();
        let moved = news(t.next().expect("t1"), after.as_ref(), &seen, Some("v1")).expect("news");
        assert_eq!(
            (moved.trigger, moved.summons.as_slice(), moved.own),
            (Trigger::Moved, &["c2".to_owned()][..], true)
        );
        assert_eq!(moved.gesture_at.as_deref(), Some("2026-11-08T09:00:00Z"));
        let activated = news(t.next().expect("t2"), after.as_ref(), &HashSet::new(), Some("v1")).expect("news");
        assert_eq!((activated.trigger, activated.own), (Trigger::Activation, true));
        assert!(news(t.next().expect("t3"), after.as_ref(), &HashSet::new(), None).is_none(), "not activated");
        let late = stamp("2026-11-08T10:00:00Z");
        assert!(news(thread::threads(&body).remove(0), late.as_ref(), &HashSet::new(), None).is_none());
    }

    /// Claude Code's first sight of a thread: after the first read an
    /// activated thread is taken up as an activation, whatever its times;
    /// the first read of an explicit watch picks up only recent summons.
    #[test]
    fn first_sight_and_recent_pickup() {
        let now = millis("2026-11-08T09:05:00Z").expect("now");
        let body = serde_json::json!({ "threads": [
            { "id": "t1", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
                { "id": "c1", "author": { "account": "u_1" }, "text": "old", "to_claude_at": "2026-11-08T08:00:00Z" },
                { "id": "c2", "author": { "account": "u_1" }, "text": "new", "to_claude_at": "2026-11-08T09:00:00Z" }
            ]},
            { "id": "t2", "comments": [
                { "id": "c3", "author": { "account": "u_1" }, "text": "x", "to_claude_at": "2026-11-08T09:00:00Z" }
            ]},
            { "id": "t3", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
                { "id": "c4", "author": { "account": "u_1" }, "text": "q", "to_claude_at": "2026-11-08T09:00:00Z" },
                { "id": "c5", "author": { "account": "u_9", "role": "assistant" }, "text": "a",
                  "created_at": "2026-11-08T09:01:00Z" }
            ]}
        ]});
        let threads = || thread::threads(&body).into_iter();
        let sighted: Vec<Fresh> = threads().filter_map(|t| first_sight(t, now, None)).collect();
        assert_eq!(sighted.len(), 2, "the thread that is not activated is only learnt");
        assert!(sighted.iter().all(|f| f.trigger == Trigger::Activation && f.summons.is_empty()));
        let picked: Vec<Fresh> = threads().filter_map(|t| picked_up(t, now, None)).collect();
        assert_eq!(picked.len(), 1, "an answered summons is not picked up");
        assert_eq!((picked[0].trigger, picked[0].summons.as_slice()), (Trigger::Fresh, &["c2".to_owned()][..]));
        let later = now + RECENT_MS + 300_001;
        assert!(threads().filter_map(|t| picked_up(t, later, None)).next().is_none(), "ten minutes on");
        assert_eq!(millis("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(millis("2000-03-01T00:00:00Z"), Some(951_868_800_000));
    }
}
