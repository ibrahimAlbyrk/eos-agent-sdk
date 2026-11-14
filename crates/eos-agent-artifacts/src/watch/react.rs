//! Claude Code's guards on the work it does unprompted on comments sent to
//! Claude, per artifact: an hourly cap on the threads it takes up (comments
//! the user sent themselves don't count), a loop breaker for a thread that
//! keeps summoning it right after each automatic answer, and a pause once
//! the session's gate keeps refusing what it composed.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Claude Code's `Ur`: an answer this soon after the last one on the same
/// thread counts toward the loop breaker.
const LOOP_WINDOW: Duration = Duration::from_secs(30);
/// Its `Ar`: so many such answers in a row open the breaker.
const LOOP_LIMIT: u32 = 3;
/// Its `Nr`: so many refused posts in a row pause a thread.
const DENIAL_LIMIT: u32 = 3;
const HOUR: Duration = Duration::from_secs(3600);
const ARTIFACTS: usize = 512;
const THREADS: usize = 2000;

/// What one artifact's comment work has done lately.
#[derive(Debug, Default)]
pub(super) struct ArtifactReact {
    turns: VecDeque<Instant>,
    cap_noticed: bool,
    /// Said once that replies wait while the session is in plan mode.
    pub(super) plan_noticed: bool,
    /// Said once that auto mode turned a reply down.
    pub(super) auto_mode_noticed: bool,
    /// The last permission check allowed a reply (a cap notice needs it).
    pub(super) last_probe_allowed: bool,
    pub(super) pipeline_denied_noticed: bool,
    /// The fence around the page's source for one version (Claude Code's
    /// `sourceFence`: one nonce per version, so the cached block stays the same).
    source_fence: Option<(String, String)>,
    /// A full rewrite ran out of room on this version with this output
    /// limit (Claude Code's `editUnavailable`): not offered again for it.
    pub(super) rewrite_unavailable: Option<(String, u32)>,
    threads: HashMap<String, ThreadReact>,
}

#[derive(Debug, Default)]
struct ThreadReact {
    last_auto_reply_at: Option<Instant>,
    consecutive_auto: u32,
    breaker_open: bool,
    consecutive_denials: u32,
}

/// What is known of a thread about to be taken up.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Take {
    /// A new human comment (or one sent again).
    pub(super) fresh_human: bool,
    /// The user sent it (or activated the thread) themselves.
    pub(super) own: bool,
    /// The thread goes to the session (it publishes from a local file).
    pub(super) handed_over: bool,
    /// The session is in plan mode (the cap's notice waits).
    pub(super) plan: bool,
}

/// Whether a thread may be taken up now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admit {
    /// Go ahead; `looping` when it follows the thread's last answer closely.
    Go { looping: bool },
    /// The loop breaker is open for this thread.
    Breaker,
    /// The posts keep being refused; `notice` the first time.
    Denied { notice: bool },
    /// The hourly cap is reached; `notice` the first time.
    Cap { notice: bool },
}

#[derive(Debug, Default)]
pub(super) struct Reacts {
    map: Mutex<HashMap<String, ArtifactReact>>,
}

impl Reacts {
    pub(super) fn with<T>(&self, slug: &str, f: impl FnOnce(&mut ArtifactReact) -> T) -> T {
        let mut map = self.lock();
        if map.len() >= ARTIFACTS && !map.contains_key(slug) {
            map.clear();
        }
        f(map.entry(slug.to_owned()).or_default())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ArtifactReact>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ArtifactReact {
    fn thread(&mut self, id: &str) -> &mut ThreadReact {
        if self.threads.len() >= THREADS && !self.threads.contains_key(id) {
            self.threads.clear();
        }
        self.threads.entry(id.to_owned()).or_default()
    }

    /// Claude Code's checks before a thread is taken up, in its order: a new
    /// human comment resets the breaker unless it came right after an
    /// answer; an open breaker or repeated refusals stop it; then the hourly
    /// cap, which comments the user sent themselves (`own`) don't count
    /// toward, and which only the first time says so.
    pub(super) fn admit(&mut self, thread: &str, take: Take, per_hour: usize, now: Instant) -> Admit {
        let Take { fresh_human, own, handed_over, plan } = take;
        let t = self.thread(thread);
        let looping = t.last_auto_reply_at.is_some_and(|at| now.saturating_duration_since(at) < LOOP_WINDOW);
        if fresh_human && !looping {
            t.consecutive_auto = 0;
            t.breaker_open = false;
        }
        if t.breaker_open {
            return Admit::Breaker;
        }
        if t.consecutive_denials >= DENIAL_LIMIT && !handed_over {
            let notice = !self.pipeline_denied_noticed;
            self.pipeline_denied_noticed = true;
            return Admit::Denied { notice };
        }
        if !own {
            self.turns.retain(|at| now.saturating_duration_since(*at) < HOUR);
            if self.turns.len() >= per_hour {
                let notice = self.last_probe_allowed && !self.cap_noticed && !plan;
                self.cap_noticed |= notice;
                return Admit::Cap { notice };
            }
            if self.turns.is_empty() {
                self.cap_noticed = false;
                self.plan_noticed = false;
            }
            self.turns.push_back(now);
        }
        Admit::Go { looping }
    }

    /// Claude Code's `ra`: the nonce for `ver`'s source, kept while none of
    /// `texts` contains it.
    pub(super) fn fence_for(&mut self, ver: &str, texts: &[&str]) -> String {
        if let Some((v, nonce)) = &self.source_fence
            && v == ver
            && !texts.iter().any(|t| t.contains(nonce.as_str()))
        {
            return nonce.clone();
        }
        let nonce = super::frame::nonce();
        self.source_fence = Some((ver.to_owned(), nonce.clone()));
        nonce
    }

    /// The thread is being worked on: one more answer in a loop opens the
    /// breaker at the third.
    pub(super) fn working(&mut self, thread: &str, looping: bool) {
        let t = self.thread(thread);
        if looping {
            t.consecutive_auto += 1;
            if t.consecutive_auto >= LOOP_LIMIT {
                t.breaker_open = true;
            }
        }
    }

    /// An answer reached the thread (or the session took it over).
    pub(super) fn answered(&mut self, thread: &str, now: Instant, reset_denials: bool) {
        self.thread(thread).last_auto_reply_at = Some(now);
        if reset_denials {
            self.threads.values_mut().for_each(|t| t.consecutive_denials = 0);
            self.pipeline_denied_noticed = false;
        }
    }

    /// The session's gate refused a composed post.
    pub(super) fn denied(&mut self, thread: &str) {
        self.thread(thread).consecutive_denials += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(fresh_human: bool, own: bool, handed_over: bool) -> Take {
        Take { fresh_human, own, handed_over, plan: false }
    }

    #[test]
    fn the_hourly_cap_spares_the_users_own_comments_and_notices_once() {
        let mut a = ArtifactReact { last_probe_allowed: true, ..ArtifactReact::default() };
        let t0 = Instant::now();
        for i in 0..2 {
            assert!(matches!(a.admit(&format!("t{i}"), take(true, false, false), 2, t0), Admit::Go { .. }));
        }
        assert_eq!(a.admit("t2", take(true, false, false), 2, t0), Admit::Cap { notice: true });
        assert_eq!(a.admit("t3", take(true, false, false), 2, t0), Admit::Cap { notice: false });
        assert!(matches!(a.admit("t4", take(true, true, false), 2, t0), Admit::Go { .. }), "the user's own send");
        assert!(matches!(a.admit("t5", take(true, false, false), 2, t0 + HOUR), Admit::Go { .. }), "an hour later");
    }

    #[test]
    fn a_thread_answered_three_times_in_quick_succession_trips_the_breaker() {
        let mut a = ArtifactReact::default();
        let t0 = Instant::now();
        let mut at = t0;
        for _ in 0..3 {
            let Admit::Go { looping } = a.admit("t", take(true, false, false), 60, at) else { panic!("admitted") };
            a.working("t", looping);
            a.answered("t", at, true);
            at += Duration::from_secs(5);
        }
        assert_eq!(a.admit("t", take(true, false, false), 60, at), Admit::Go { looping: true });
        a.working("t", true);
        assert_eq!(a.admit("t", take(true, false, false), 60, at), Admit::Breaker);
        // A human comment well after the last answer closes it again.
        assert!(matches!(a.admit("t", take(true, false, false), 60, at + LOOP_WINDOW), Admit::Go { looping: false }));

        for _ in 0..DENIAL_LIMIT {
            a.denied("u");
        }
        assert_eq!(a.admit("u", take(true, false, false), 60, at), Admit::Denied { notice: true });
        assert_eq!(a.admit("u", take(true, false, false), 60, at), Admit::Denied { notice: false });
        assert!(matches!(a.admit("u", take(true, false, true), 60, at), Admit::Go { .. }), "a hand-over is not a post");
        a.answered("t", at, true);
        assert!(
            matches!(a.admit("u", take(true, false, false), 60, at), Admit::Go { .. }),
            "a post anywhere resumes it"
        );
    }
}
