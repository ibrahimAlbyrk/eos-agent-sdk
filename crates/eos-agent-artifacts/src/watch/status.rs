//! Claude Code's `summon-status` updates: what happened to a comment sent to
//! Claude (`working`, `declined`, `failed`), posted to
//! `POST /api/frame/comments/<slug>/<thread>/summon-status` so the page can
//! show it. `{status, reason?, covers_reply_id?, gesture_at?, sid, seq}`:
//! `sid` names this session's stream for the artifact (an HMAC of the
//! session id and the slug, 16 hex digits), `seq` orders it (at least the
//! current second in milliseconds, then one more each time). A route the
//! server does not have (404) is not asked again for an hour.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::client::FrameClient;

const ROUTE_ABSENT_FOR: Duration = Duration::from_secs(3600);
const TIMEOUT: Duration = Duration::from_secs(5);
/// JavaScript's `Number.MAX_SAFE_INTEGER`.
const MAX_SEQ: u64 = (1 << 53) - 1;
const SLUGS: usize = 1024;

/// One update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Working,
    Declined(&'static str),
    Failed(&'static str),
}

impl Status {
    fn parts(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Working => ("working", None),
            Self::Declined(why) => ("declined", Some(why)),
            Self::Failed(why) => ("failed", Some(why)),
        }
    }
}

/// The summon an update is about.
#[derive(Debug, Clone, Default)]
pub(super) struct Summon {
    pub(super) slug: String,
    pub(super) thread: String,
    /// The newest time it was sent to Claude (or the thread activated).
    pub(super) gesture_at: Option<String>,
    /// The reply that already answers it.
    pub(super) covers_reply_id: Option<String>,
}

/// The process's sender of updates.
#[derive(Debug)]
pub(super) struct StatusSender {
    key: [u8; 32],
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    sids: HashMap<String, String>,
    seqs: HashMap<String, u64>,
    route_absent_since: Option<Instant>,
}

impl Default for StatusSender {
    fn default() -> Self {
        let mut key = [0u8; 32];
        key.iter_mut().for_each(|b| *b = fastrand::u8(..));
        Self { key, state: Mutex::default() }
    }
}

impl StatusSender {
    /// Sends `status` for `summon` in the background; false when it was not
    /// sent (the route is known to be absent).
    pub(super) fn send(self: &Arc<Self>, client: &Arc<FrameClient>, summon: &Summon, status: Status) -> bool {
        let Some(body) = self.body(client.session_id().unwrap_or_default(), summon, status, now_ms()) else {
            return false;
        };
        let (me, client, slug, thread) =
            (Arc::clone(self), Arc::clone(client), summon.slug.clone(), summon.thread.clone());
        tokio::spawn(async move {
            let sent = tokio::time::timeout(TIMEOUT, client.summon_status(&slug, &thread, &body)).await;
            if let Ok(Ok(404)) = sent {
                me.lock().route_absent_since = Some(Instant::now());
            }
        });
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claude Code's `i_`: the body of one update, or None while the route
    /// is known to be absent.
    fn body(&self, session: &str, summon: &Summon, status: Status, now_ms: u64) -> Option<Value> {
        let mut state = self.lock();
        if let Some(since) = state.route_absent_since {
            if since.elapsed() < ROUTE_ABSENT_FOR {
                return None;
            }
            state.route_absent_since = None;
        }
        if state.sids.len() >= SLUGS && !state.sids.contains_key(&summon.slug) {
            state.sids.clear();
        }
        let sid =
            state.sids.entry(summon.slug.clone()).or_insert_with(|| sid(&self.key, session, &summon.slug)).clone();
        let seq = state.seqs.get(&sid).copied().unwrap_or(0).max(now_ms / 1000 * 1000);
        if seq > MAX_SEQ {
            return None;
        }
        state.seqs.insert(sid.clone(), seq + 1);
        let (name, reason) = status.parts();
        let mut body = Map::new();
        body.insert("status".into(), json!(name));
        if let Some(reason) = reason {
            body.insert("reason".into(), json!(reason));
        }
        if let Some(id) = summon.covers_reply_id.as_deref().filter(|id| valid_id(id)) {
            body.insert("covers_reply_id".into(), json!(id));
        }
        if let Some(at) = &summon.gesture_at {
            body.insert("gesture_at".into(), json!(at));
        }
        body.insert("sid".into(), json!(sid));
        body.insert("seq".into(), json!(seq));
        Some(Value::Object(body))
    }
}

/// Claude Code's `gr`: ids the frame API names threads and comments with.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// HMAC-SHA256 of `session` + `slug`, the first 16 hex digits.
fn sid(key: &[u8; 32], session: &str, slug: &str) -> String {
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(session.as_bytes());
    inner.update(slug.as_bytes());
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    hex::encode(outer.finalize())[..16].to_owned()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_carry_a_stable_sid_and_a_rising_seq() {
        let sender = StatusSender::default();
        let summon = Summon {
            slug: "s1".into(),
            thread: "t1".into(),
            gesture_at: Some("2026-11-08T09:00:00Z".into()),
            covers_reply_id: Some("not an id!".into()),
        };
        let a = sender.body("sess", &summon, Status::Working, 1_794_000_000_123).expect("body");
        let b = sender.body("sess", &summon, Status::Declined("hourly_cap"), 1_794_000_000_456).expect("body");
        assert_eq!(a["status"], "working");
        assert!(a.get("reason").is_none() && a.get("covers_reply_id").is_none());
        assert_eq!(a["gesture_at"], "2026-11-08T09:00:00Z");
        assert_eq!(a["sid"].as_str().map(str::len), Some(16));
        assert_eq!(a["sid"], b["sid"]);
        assert_eq!((a["seq"].as_u64(), b["seq"].as_u64()), (Some(1_794_000_000_000), Some(1_794_000_000_001)));
        assert_eq!((b["status"].as_str(), b["reason"].as_str()), (Some("declined"), Some("hourly_cap")));
        // A later second starts there.
        let c = sender.body("sess", &summon, Status::Failed("post_failed"), 1_794_000_009_000).expect("body");
        assert_eq!(c["seq"].as_u64(), Some(1_794_000_009_000));

        sender.lock().route_absent_since = Some(Instant::now());
        assert!(sender.body("sess", &summon, Status::Working, 0).is_none(), "an absent route is not asked again");
        assert!(valid_id("0f8e2a9c-1b3d") && !valid_id("c9 x"));
    }
}
