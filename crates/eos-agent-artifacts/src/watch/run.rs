//! One artifact's live connection: boot for a token, open the channel,
//! follow it, reconnect with backoff until nobody watches.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use super::comments::schedule_comment_check;
use super::{Inner, WatchConfig, drop_subscriber, newer, notices, socket};
use crate::client::FrameError;

impl Inner {
    /// Sends notes (outside the lock) and drops agents that are gone.
    pub(super) async fn deliver(&self, slug: &str, notes: Vec<(Arc<str>, String, bool)>) {
        for (agent, text, wake) in notes {
            if !self.notifier.notify(&agent, text, wake).await {
                drop_subscriber(&mut self.lock(), slug, &agent);
            }
        }
    }

    /// Ends a watch on its own, telling its agents why.
    pub(super) async fn end(&self, slug: &str, why: &str) {
        let Some(watch) = self.lock().remove(slug) else { return };
        let text = notices::ended(&watch.url, why);
        for s in watch.subscribers {
            self.notifier.notify(&s.agent, text.clone(), false).await;
        }
    }

    pub(super) fn set_connected(&self, slug: &str, on: bool) -> bool {
        match self.lock().get_mut(slug) {
            Some(w) => {
                w.connected = on;
                if on {
                    w.problem = None;
                }
                true
            }
            None => false,
        }
    }

    pub(super) fn set_problem(&self, slug: &str, problem: String) {
        if let Some(w) = self.lock().get_mut(slug) {
            w.problem = Some(problem);
        }
    }

    pub(super) async fn version(&self, slug: &str, ver: &str) {
        let notes = {
            let mut watches = self.lock();
            let Some(watch) = watches.get_mut(slug) else { return };
            if watch.own.iter().any(|v| v == ver) {
                return;
            }
            let url = watch.url.clone();
            let mut notes = Vec::new();
            for s in &mut watch.subscribers {
                let behind = s.known.as_deref().is_none_or(|k| newer(ver, k));
                let told = s.told.as_deref().is_some_and(|t| !newer(ver, t));
                if behind && !told {
                    s.told = Some(ver.to_owned());
                    notes.push((Arc::clone(&s.agent), notices::republished(&url, ver), false));
                }
            }
            notes
        };
        self.deliver(slug, notes).await;
    }
}

/// Claude Code's lease on a live-channel token: reused across reconnects
/// while it has time left, renewed when it may be, else booted again.
#[derive(Debug, Clone)]
struct Lease {
    token: String,
    ver: Option<String>,
    expires_ms: u64,
    opened: bool,
    failed_dials: u32,
}

/// The close code the channel sends when its token ended.
const EXPIRED: u16 = 4001;
/// Without its own end, a token lasts an hour; whatever it says, between half
/// an hour and an hour.
const LEASE_DEFAULT_MS: u64 = 3_600_000;
const LEASE_MIN_MS: u64 = 1_800_000;

fn lease_end(received_ms: u64, exp_secs: Option<f64>) -> u64 {
    let left = exp_secs.map_or(LEASE_DEFAULT_MS as i128, |s| (s * 1000.0) as i128 - i128::from(received_ms));
    received_ms + left.clamp(LEASE_MIN_MS as i128, LEASE_DEFAULT_MS as i128) as u64
}

/// What one artifact's connection keeps between attempts.
#[derive(Debug, Default)]
struct Line {
    lease: Option<Lease>,
    renewable: bool,
    failures: u32,
    /// The API has been unavailable since this run of attempts began.
    stall: Option<StallRun>,
}

/// Claude Code's stall run: consecutive attempts the API was unavailable for.
#[derive(Debug, Clone, Copy)]
struct StallRun {
    since: Instant,
    last: Instant,
    /// The watch ends once the run lasts this long.
    out_after: Duration,
}

/// How an attempt ended.
#[derive(Debug, Default)]
struct Ended {
    opened: bool,
    uptime: Duration,
    code: Option<u16>,
    /// The attempt used a leased token.
    reused: bool,
    /// A dial was made with a token (not a boot that failed).
    dialed: bool,
    /// The API was unavailable: a 5xx, 429 or 408 answer, Cloudflare's
    /// challenge, or no answer within the stall threshold.
    stalled: bool,
    /// The upgrade itself was turned away that way (the token is not to blame).
    shed: bool,
    cf_mitigated: bool,
}

/// Claude Code's `J8`: statuses that say the API is unavailable, not refusing.
fn unavailable(status: u16) -> bool {
    status >= 500 || status == 429 || status == 408
}

/// One artifact's connection, for as long as anyone watches it.
pub(super) async fn run(inner: Weak<Inner>, slug: String) {
    let mut line = Line::default();
    loop {
        let Some(this) = inner.upgrade() else { return };
        let config = this.config.clone();
        let ended = match connect_once(&this, &slug, &mut line).await {
            Ok(ended) => ended,
            Err(Stop::Gone) => {
                this.end(&slug, "the artifact no longer exists, or this account can no longer open it").await;
                return;
            }
            Err(Stop::NoToken) => {
                this.end(&slug, "the server minted no live-channel credential for this artifact").await;
                return;
            }
            Err(Stop::Unwatched) => return,
        };
        if !this.set_connected(&slug, false) {
            return;
        }
        let Some(wait) = next_wait(&config, &mut line, &ended, Instant::now(), now_ms()) else {
            let why = if ended.opened { "could not reconnect" } else { "could not connect" };
            this.end(&slug, why).await;
            return;
        };
        drop(this);
        tokio::time::sleep(wait).await;
    }
}

/// Claude Code's `en`: settles the lease, counts the attempt and picks the
/// wait before the next one; None when the watch gives up. Attempts the API
/// was unavailable for don't count as failures: they wait longer instead
/// (30–60 s, then 2–4 minutes while the token can't be reused), and the
/// watch ends once that has gone on for half an hour or more.
fn next_wait(config: &WatchConfig, line: &mut Line, ended: &Ended, now: Instant, now_ms: u64) -> Option<Duration> {
    let expired = ended.code == Some(EXPIRED);
    if ended.dialed && !ended.shed {
        settle_lease(line, ended, expired, config.min_uptime);
    }
    let mut continuing = false;
    let mut stalled_out = false;
    if ended.stalled {
        let run = match line.stall {
            Some(mut run) if now.saturating_duration_since(run.last) <= stall_gap(config) => {
                continuing = true;
                run.last = now;
                run
            }
            _ => StallRun {
                since: now,
                last: now,
                out_after: config.max_stall + config.stall_out_jitter.mul_f64(fastrand::f64()),
            },
        };
        stalled_out = now.saturating_duration_since(run.since) >= run.out_after;
        line.stall = Some(run);
    } else {
        line.stall = None;
        // Claude Code's count of attempts that did not last.
        if ended.uptime >= config.min_uptime {
            line.failures = 0;
        } else if !(ended.reused && !ended.opened) {
            line.failures += 1;
        }
    }
    if line.failures > config.give_up_after || stalled_out {
        return None;
    }
    if ended.stalled {
        let soon = now_ms + config.stall.0.as_millis() as u64;
        let reusable = line
            .lease
            .as_ref()
            .is_some_and(|l| l.expires_ms.saturating_sub(soon) >= config.lease_margin.as_millis() as u64);
        let long = continuing && (ended.cf_mitigated || !reusable);
        let (low, high) = if long { config.long_stall } else { config.stall };
        return Some(low + high.saturating_sub(low).mul_f64(fastrand::f64()));
    }
    let spread = ended.uptime >= config.min_uptime && ended.code.is_some() && !expired;
    Some(delay(config, line.failures, spread))
}

/// A stall more than this after the last one starts a new run (Claude Code's
/// `ao`: the longest stall wait, two token renewals of 15 s, the handshake
/// deadline of 150 s and a minute).
fn stall_gap(config: &WatchConfig) -> Duration {
    config.stall.1.max(config.long_stall.1) + Duration::from_secs(240)
}

/// Claude Code's `Eqo`: an expired or refused lease is dropped; a healthy
/// one is kept; a dial that failed counts, three drop it. A lease that never
/// opened takes the right to renew with it.
fn settle_lease(line: &mut Line, ended: &Ended, expired: bool, min_uptime: Duration) {
    let Some(lease) = line.lease.as_mut() else { return };
    let refused = ended.reused && !ended.opened && !ended.stalled;
    let keep = if expired || refused {
        false
    } else if ended.opened && ended.uptime >= min_uptime {
        lease.failed_dials = 0;
        true
    } else {
        lease.failed_dials += 1;
        lease.failed_dials < 3
    };
    if !keep {
        if !lease.opened {
            line.renewable = false;
        }
        line.lease = None;
    }
}

enum Stop {
    Gone,
    NoToken,
    Unwatched,
}

/// A token for the channel (and whether it is the lease), or a boot that
/// failed (and whether the API was unavailable).
enum Token {
    Got(Lease, bool),
    Failed { stalled: bool },
}

/// A token for the channel: the lease while it has time left, else a renewal
/// when the server allows one, else a boot.
async fn token(inner: &Arc<Inner>, slug: &str, line: &mut Line) -> Result<Token, Stop> {
    let now = now_ms();
    if let Some(lease) = &line.lease
        && lease.expires_ms.saturating_sub(now) >= inner.config.lease_margin.as_millis() as u64
    {
        return Ok(Token::Got(lease.clone(), true));
    }
    let mut boot = None;
    if line.renewable {
        match inner.client.renew_watch_token(slug).await {
            Ok(b) if b.ver.is_some() && b.subscription_token.is_some() => {
                boot = Some(b);
                line.renewable = true;
            }
            _ => line.renewable = false,
        }
    }
    let started = Instant::now();
    let boot = match boot {
        Some(b) => b,
        None => match inner.client.watch_boot(slug).await {
            Ok(Some(b)) => {
                line.renewable = b.watch_token_renew_enabled == Some(true);
                if let (Some(title), Some(w)) = (&b.title, inner.lock().get_mut(slug)) {
                    w.title = Some(title.clone());
                }
                b
            }
            Ok(None) | Err(FrameError::Status { status: 403, .. }) => return Err(Stop::Gone),
            Err(e) => {
                // Claude Code's `L7o`: unavailable, or no answer for long.
                let stalled = match &e {
                    FrameError::Status { status, .. } => unavailable(*status),
                    FrameError::Network(_) => started.elapsed() >= inner.config.stall_threshold,
                    _ => false,
                };
                inner.set_problem(slug, e.to_string());
                return Ok(Token::Failed { stalled });
            }
        },
    };
    let token = boot.subscription_token.clone().ok_or(Stop::NoToken)?;
    let lease = Lease {
        token,
        ver: boot.ver.clone(),
        expires_ms: lease_end(now_ms(), boot.subscription_token_exp),
        opened: false,
        failed_dials: 0,
    };
    line.lease = Some(lease.clone());
    Ok(Token::Got(lease, false))
}

/// Gets a token, opens the channel and follows it until it drops.
async fn connect_once(inner: &Arc<Inner>, slug: &str, line: &mut Line) -> Result<Ended, Stop> {
    let (lease, reused) = match token(inner, slug, line).await? {
        Token::Got(lease, reused) => (lease, reused),
        Token::Failed { stalled } => return Ok(Ended { stalled, ..Ended::default() }),
    };
    let primed = inner.lock().get(slug).map(|w| w.comments_primed).ok_or(Stop::Unwatched)?;
    if !primed {
        inner.comments(slug).await;
    }
    let config = &inner.config;
    let ua = inner.client.user_agent().to_owned();
    let dial = Instant::now();
    let mut sock =
        match socket::open(&inner.http, &config.live_origin, slug, &lease.token, &ua, config.connect_timeout).await {
            Ok(sock) => sock,
            Err(e) => {
                // Claude Code: an upgrade turned away as unavailable (or by
                // Cloudflare), or a dial that hung, is a stall.
                let (shed, cf_mitigated) = match &e {
                    socket::OpenError::Refused { status, cf_mitigated } => {
                        (unavailable(*status) || *cf_mitigated, *cf_mitigated)
                    }
                    socket::OpenError::Network(_) => (false, false),
                };
                let stalled = shed || dial.elapsed() >= config.stall_threshold;
                inner.set_problem(slug, e.to_string());
                return Ok(Ended { reused, dialed: true, stalled, shed, cf_mitigated, ..Ended::default() });
            }
        };
    if !inner.set_connected(slug, true) {
        return Err(Stop::Unwatched);
    }
    if let Some(held) = line.lease.as_mut() {
        held.opened = true;
    }
    if let Some(ver) = lease.ver.as_ref().filter(|_| !reused) {
        // An agent that watched without reading has the live version; what
        // went live while the line was down is news to the others.
        if let Some(watch) = inner.lock().get_mut(slug) {
            for s in watch.subscribers.iter_mut().filter(|s| s.known.is_none()) {
                s.known = Some(ver.clone());
            }
        }
        inner.version(slug, ver).await;
    }
    let opened = Instant::now();
    let mut code = None;
    while let Some(event) = socket::next(&mut sock, config.idle_ping, &mut code).await {
        match event {
            socket::Event::Version(ver) => inner.version(slug, &ver).await,
            socket::Event::Comment => schedule_comment_check(inner, slug),
            socket::Event::Other => {}
        }
    }
    Ok(Ended { opened: true, uptime: opened.elapsed(), code, reused, dialed: true, ..Ended::default() })
}

/// Claude Code's waits between attempts: a line that lasted and was closed
/// comes back at a random point of the spread window (so many watches that
/// dropped together don't return together), else after a doubling backoff
/// with ±50 % jitter.
fn delay(config: &WatchConfig, failures: u32, spread: bool) -> Duration {
    if spread {
        let (low, high) = config.spread;
        return low + (high.saturating_sub(low)).mul_f64(fastrand::f64());
    }
    let (first, most) = config.backoff;
    let base = first.saturating_mul(1u32 << failures.min(5)).min(most);
    base.mul_f64(0.5 + fastrand::f64())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_grow_to_their_cap() {
        let config = WatchConfig::default();
        assert!(delay(&config, 0, false) <= Duration::from_millis(1500));
        assert!(delay(&config, 20, false) <= Duration::from_secs(45));
        assert!(delay(&config, 20, false) >= Duration::from_secs(15));
        let spread = delay(&config, 0, true);
        assert!(spread >= Duration::from_secs(1) && spread <= Duration::from_secs(20));
    }

    #[test]
    fn leases_last_half_an_hour_to_an_hour() {
        assert_eq!(lease_end(1_000, None), 1_000 + LEASE_DEFAULT_MS);
        assert_eq!(lease_end(0, Some(600.0)), LEASE_MIN_MS);
        assert_eq!(lease_end(0, Some(2_700.0)), 2_700_000);
        assert_eq!(lease_end(0, Some(99_999.0)), LEASE_DEFAULT_MS);
    }

    #[test]
    fn leases_settle_like_claude_code() {
        let lease = Lease { token: "t".into(), ver: None, expires_ms: 0, opened: true, failed_dials: 0 };
        let minute = Duration::from_secs(60);
        let ended = |opened, secs, reused| Ended {
            opened,
            uptime: Duration::from_secs(secs),
            reused,
            dialed: true,
            ..Ended::default()
        };
        let mut line = Line { lease: Some(lease.clone()), renewable: true, ..Line::default() };
        settle_lease(&mut line, &ended(true, 120, true), false, minute);
        assert!(line.lease.is_some(), "a healthy line keeps its lease");
        settle_lease(&mut line, &ended(true, 120, true), true, minute);
        assert!(line.lease.is_none() && line.renewable, "an expired one is renewed next");
        let mut line = Line { lease: Some(Lease { opened: false, ..lease }), renewable: true, ..Line::default() };
        settle_lease(&mut line, &ended(false, 0, true), false, minute);
        assert!(line.lease.is_none() && !line.renewable, "a refused token that never opened is booted again");
    }

    fn lease_until(expires_ms: u64) -> Lease {
        Lease { token: "t".into(), ver: None, expires_ms, opened: true, failed_dials: 0 }
    }

    /// Claude Code waits 30–60 s while the API answers 5xx/429 (2–4 minutes
    /// once the stall goes on and the token can't be reused), never counts
    /// those attempts as failures, and ends the watch after half an hour to
    /// three quarters of an hour of them.
    #[test]
    fn an_unavailable_api_gets_longer_waits_and_no_failures() {
        let config = WatchConfig { give_up_after: 2, ..WatchConfig::default() };
        let shed = Ended { reused: true, dialed: true, stalled: true, shed: true, ..Ended::default() };
        let t0 = Instant::now();
        let hour_ms = 3_600_000;
        let mut line = Line { lease: Some(lease_until(hour_ms)), failures: 2, ..Line::default() };
        for i in 0..5u64 {
            let wait = next_wait(&config, &mut line, &shed, t0 + Duration::from_secs(60 * i), 0).expect("keeps going");
            assert!((30..=60).contains(&wait.as_secs()), "{wait:?}");
        }
        assert_eq!(line.failures, 2, "stalls are not failures");
        assert!(line.lease.as_ref().is_some_and(|l| l.failed_dials == 0), "a shed upgrade leaves the lease alone");

        // The token has too little time left: the run waits long.
        let mut line = Line { lease: Some(lease_until(200_000)), ..Line::default() };
        let first = next_wait(&config, &mut line, &shed, t0, 0).expect("first");
        assert!((30..=60).contains(&first.as_secs()), "the first stall of a run is short");
        let second = next_wait(&config, &mut line, &shed, t0 + first, 0).expect("second");
        assert!((120..=240).contains(&second.as_secs()), "{second:?}");

        // Cloudflare's challenge waits long once the run goes on.
        let cf = Ended { cf_mitigated: true, ..shed };
        let mut line = Line { lease: Some(lease_until(hour_ms)), ..Line::default() };
        next_wait(&config, &mut line, &cf, t0, 0).expect("first");
        assert!(next_wait(&config, &mut line, &cf, t0 + Duration::from_secs(40), 0).expect("second").as_secs() >= 120);

        // A boot answered 503 is a stall too; one that went on for 45 minutes ends the watch.
        let boot = Ended { stalled: true, ..Ended::default() };
        let mut line = Line::default();
        let mut at = t0;
        let mut ended = false;
        for _ in 0..100 {
            match next_wait(&config, &mut line, &boot, at, 0) {
                Some(wait) => at += wait,
                None => {
                    ended = true;
                    break;
                }
            }
        }
        let lasted = at - t0;
        assert!(
            ended
                && lasted >= config.max_stall
                && lasted <= config.max_stall + config.stall_out_jitter + Duration::from_secs(240)
        );

        // A gap longer than the run's window starts a new run.
        let mut line = Line::default();
        next_wait(&config, &mut line, &boot, t0, 0);
        next_wait(&config, &mut line, &boot, t0 + Duration::from_secs(1000), 0);
        assert_eq!(line.stall.map(|r| r.since), Some(t0 + Duration::from_secs(1000)));

        // A normal failure ends the run and counts again.
        let failed = Ended { dialed: true, ..Ended::default() };
        next_wait(&config, &mut line, &failed, t0 + Duration::from_secs(1010), 0);
        assert!(line.stall.is_none() && line.failures == 1);
        assert!(unavailable(503) && unavailable(429) && unavailable(408) && !unavailable(403));
    }
}
