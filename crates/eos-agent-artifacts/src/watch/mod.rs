//! Artifact watching, as Claude Code does it: after a publish (or an
//! explicit `watch`), the session follows the artifact's live channel. A
//! version published from elsewhere leaves the agent a note (no turn starts);
//! a comment sent to Claude wakes the agent that armed replies.
//!
//! One service per process, one live connection per watched artifact,
//! shared by every agent that watches it and made on the sign-in of the
//! agent that answers its comments. Each connection reconnects with
//! backoff and ends on its own once nobody watches; the tables are bounded.

mod ack;
mod anchor;
mod answer;
mod coalesce;
mod comments;
mod edit;
mod frame;
mod notices;
mod react;
mod replies;
mod run;
mod socket;
mod source;
mod status;
mod thread;

use comments::Stamp;
pub use replies::{REPLY_MAX_TOKENS, REPLY_SYSTEM};
use run::run;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use eos_agent_core::{BoxFuture, Extensions};

use crate::client::FrameClient;
use crate::ids;
use crate::state::{ArtifactState, Known};

/// Where the watcher's notes go: the host delivers them to its agents.
pub trait WatchNotifier: Send + Sync {
    /// Leaves `text` for agent `agent_id` (with `wake`, an idle agent starts
    /// a turn for it). False when the agent is gone.
    fn notify<'a>(&'a self, agent_id: &'a str, text: String, wake: bool) -> BoxFuture<'a, bool>;

    /// Whether this agent may hold watches (Claude Code: only a main
    /// conversation, never a subagent).
    fn can_watch(&self, _agent_id: &str) -> bool {
        true
    }

    /// One request to this agent's model as Claude Code's comment composers
    /// send it: no tools, no thinking. `None` when there is no model to ask;
    /// the agent is then woken to answer the thread itself.
    fn compose<'a>(&'a self, _agent_id: &'a str, _request: &'a ComposeRequest) -> BoxFuture<'a, Option<Composed>> {
        Box::pin(std::future::ready(None))
    }

    /// The `max_tokens` this agent's model takes by default (Claude Code
    /// sizes automatic edits by it). `None`: no automatic edits.
    fn output_cap(&self, _agent_id: &str) -> Option<u32> {
        None
    }

    /// Whether this agent may make the call `tool` with `input` (the host's
    /// permission gate): each automatic reply is an `ArtifactComments`
    /// `reply`, each automatic edit an `Artifact` publish. False: nothing is
    /// done (for a reply, the session is told once).
    fn may_call<'a>(
        &'a self,
        _agent_id: &'a str,
        _tool: &'a str,
        _input: &'a serde_json::Value,
    ) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(true))
    }

    /// Claude Code's permission probe before a thread is taken up: the
    /// verdict on a reply with no text yet, unattended. By default
    /// [`may_call`](Self::may_call): allowed, or the user would have to
    /// approve it (replies are then notify-only).
    fn probe<'a>(&'a self, agent_id: &'a str, tool: &'a str, input: &'a serde_json::Value) -> BoxFuture<'a, Probe> {
        Box::pin(async move { if self.may_call(agent_id, tool, input).await { Probe::Allow } else { Probe::Ask } })
    }

    /// The agent's session is in plan mode: Claude Code takes up no comment
    /// unprompted and says so once.
    fn plan_mode(&self, _agent_id: &str) -> bool {
        false
    }
}

/// A permission probe's verdict (see [`WatchNotifier::probe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Allow,
    /// Someone would have to approve it: replies are notify-only.
    Ask,
    /// A rule refuses it.
    Deny,
    /// Claude Code's auto mode turned it down.
    DeniedByAutoMode,
}

/// A composer's request (see [`WatchNotifier::compose`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeRequest {
    pub system: String,
    /// The one user message's text blocks, in order.
    pub blocks: Vec<String>,
    /// The first block (a page's source) is marked for prompt caching.
    pub cache_first: bool,
    pub max_tokens: u32,
    /// On a model that takes an effort: this effort, and this `max_tokens`.
    pub effort: Option<(&'static str, u32)>,
    /// The small fast model (Claude Code's `ANTHROPIC_SMALL_FAST_MODEL`), not the agent's.
    pub small_model: bool,
}

/// What a composer got back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composed {
    Text(String),
    /// The answer ran into `max_tokens`.
    Truncated,
    /// The plan's usage limit is reached (a 429 `rate_limit`).
    UsageLimited,
    Failed,
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// The live channel's origin (`https://claude.ai`); the socket is
    /// `wss://…/edge-api/frame-live/<slug>/ws`.
    pub live_origin: String,
    /// Artifacts watched at once, by the whole process.
    pub max_artifacts: usize,
    /// Watches one agent holds (Claude Code: 25 per session).
    pub max_per_agent: usize,
    /// First and longest wait between reconnects (doubling, ±50 % jitter).
    pub backoff: (Duration, Duration),
    /// A watch ends after more consecutive attempts than this that did not last.
    pub give_up_after: u32,
    /// A connection open this long counted as working.
    pub min_uptime: Duration,
    /// A working connection that was closed comes back within this window.
    pub spread: (Duration, Duration),
    /// A leased token is reused while it has this long left.
    pub lease_margin: Duration,
    /// A failed attempt that took this long without an answer counts as the
    /// API being unavailable (as a 5xx, 429 or 408 answer does).
    pub stall_threshold: Duration,
    /// While the API is unavailable, attempts wait a random time in this
    /// window, or in the long one when it stays unavailable and the token
    /// can't be reused.
    pub stall: (Duration, Duration),
    pub long_stall: (Duration, Duration),
    /// A watch the API stays unavailable for this long (plus up to the
    /// jitter) ends.
    pub max_stall: Duration,
    pub stall_out_jitter: Duration,
    /// A quiet line is pinged after this long, and dropped if it stays quiet.
    pub idle_ping: Duration,
    /// Threads taken up unprompted per artifact in an hour (Claude Code: 60).
    pub auto_turns_per_hour: usize,
    /// Notices of one kind on one artifact are gathered until this long
    /// after the latest, and at most the second this long after the first.
    pub notice_settle: (Duration, Duration),
    /// Comment notes are gathered this long before reading the threads.
    pub comment_debounce: Duration,
    pub connect_timeout: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            live_origin: "https://claude.ai".into(),
            max_artifacts: 256,
            max_per_agent: 25,
            backoff: (Duration::from_secs(1), Duration::from_secs(30)),
            give_up_after: 10,
            min_uptime: Duration::from_secs(60),
            spread: (Duration::from_secs(1), Duration::from_secs(20)),
            // Claude Code: the handshake deadline (150 s), the minimum uptime and a minute.
            lease_margin: Duration::from_secs(270),
            stall_threshold: Duration::from_secs(10),
            stall: (Duration::from_secs(30), Duration::from_secs(60)),
            long_stall: (Duration::from_secs(120), Duration::from_secs(240)),
            max_stall: Duration::from_secs(1800),
            stall_out_jitter: Duration::from_secs(900),
            idle_ping: Duration::from_secs(60),
            auto_turns_per_hour: 60,
            notice_settle: (Duration::from_secs(15), Duration::from_secs(60)),
            comment_debounce: Duration::from_millis(750),
            connect_timeout: Duration::from_secs(30),
        }
    }
}

/// How a watch request went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Armed {
    /// Connecting in the background.
    Arming,
    /// The artifact's channel was already open.
    Connected,
    /// This agent may not hold watches.
    NotHere,
    Refused(String),
}

impl Armed {
    /// The subscription line of a publish result.
    pub fn line(&self) -> String {
        match self {
            Self::Arming => notices::arming(),
            Self::Connected => notices::connected(),
            Self::NotHere => notices::subagent(),
            Self::Refused(why) => notices::skipped(why),
        }
    }
}

/// One of an agent's watches, for listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRow {
    pub url: String,
    pub connected: bool,
    /// Why the last connect failed, while reconnecting.
    pub problem: Option<String>,
    /// Comments sent to Claude wake this agent.
    pub replies: bool,
}

/// The process-wide watch service. Cheap to clone.
#[derive(Clone)]
pub struct ArtifactWatcher {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ArtifactWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactWatcher").field("artifacts", &self.inner.lock().len()).finish_non_exhaustive()
    }
}

struct Inner {
    client: Arc<FrameClient>,
    /// HTTP/1.1 only: a WebSocket upgrade can't ride HTTP/2.
    http: reqwest::Client,
    notifier: Arc<dyn WatchNotifier>,
    config: WatchConfig,
    watches: Mutex<HashMap<String, Watch>>,
    /// The local file each artifact was last published from in this process.
    sources: Mutex<HashMap<String, PathBuf>>,
    /// Artifacts whose sessions were told replies are notify-only.
    notified: Mutex<HashSet<String>>,
    /// Each agent's own record of its artifacts (the Artifact tool's
    /// state): an automatic edit is published as that agent's tool call
    /// would publish it, so the agent's next publish builds on it.
    states: Mutex<HashMap<String, Weak<ArtifactState>>>,
    /// The client of each agent that carries its own sign-in.
    clients: Mutex<HashMap<String, Arc<FrameClient>>>,
    status: Arc<status::StatusSender>,
    reacts: react::Reacts,
    coalescer: coalesce::Coalescer,
}

struct Watch {
    url: String,
    /// The artifact's title, as its latest boot said.
    title: Option<String>,
    connected: bool,
    problem: Option<String>,
    subscribers: Vec<Subscriber>,
    /// Versions agents of this process published: never news to them.
    own: VecDeque<String>,
    /// Read the threads once already (the first read only learns what is there).
    comments_primed: bool,
    /// The first read picks up comments sent to Claude in the last minutes
    /// (an explicit watch, or a resumed session's).
    pick_up_recent: bool,
    /// Comments sent to Claude after this are news.
    comments_after: Option<Stamp>,
    /// The comments each thread had at the last read.
    seen: HashMap<String, HashSet<String>>,
    comment_check: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

#[derive(Debug, Clone)]
struct Subscriber {
    agent: Arc<str>,
    /// The version this agent has (read or published).
    known: Option<String>,
    /// The newest version it was told about.
    told: Option<String>,
    replies: bool,
    explicit: bool,
    since: Instant,
}

const OWN_VERSIONS: usize = 8;

impl ArtifactWatcher {
    pub fn new(client: Arc<FrameClient>, notifier: Arc<dyn WatchNotifier>, config: WatchConfig) -> Self {
        let http = reqwest::Client::builder()
            .http1_only()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        let (sources, notified) = (Mutex::default(), Mutex::default());
        let status = Arc::default();
        Self {
            inner: Arc::new(Inner {
                client,
                http,
                notifier,
                config,
                watches: Mutex::default(),
                sources,
                notified,
                states: Mutex::default(),
                clients: Mutex::default(),
                status,
                reacts: react::Reacts::default(),
                coalescer: coalesce::Coalescer::default(),
            }),
        }
    }

    /// Starts (or joins) watching `slug` for `agent`. `version` is what the
    /// agent has; `published` marks it as the agent's own; `explicit` is a
    /// watch the agent asked for (it is not displaced by later publishes).
    pub fn watch(&self, agent: &str, slug: &str, version: Option<&str>, published: bool, explicit: bool) -> Armed {
        let inner = &self.inner;
        if !inner.notifier.can_watch(agent) {
            return Armed::NotHere;
        }
        let mut watches = inner.lock();
        let held: Vec<String> = watches
            .iter()
            .filter(|(s, w)| *s != slug && w.subscribers.iter().any(|s| &*s.agent == agent))
            .map(|(s, _)| s.clone())
            .collect();
        if held.len() >= inner.config.max_per_agent {
            // A publish makes room by dropping the agent's oldest implicit watch.
            let oldest = held
                .iter()
                .filter_map(|s| {
                    let sub = watches[s].subscribers.iter().find(|x| &*x.agent == agent)?;
                    (!sub.explicit).then_some((sub.since, s.clone()))
                })
                .min();
            match oldest {
                Some((_, victim)) if !explicit => drop_subscriber(&mut watches, &victim, agent),
                _ => {
                    return Armed::Refused(format!(
                        "this session already holds its maximum of {} artifact watches; unwatch one first",
                        inner.config.max_per_agent
                    ));
                }
            }
        }
        let (exists, connected) = watches.get(slug).map_or((false, false), |w| (true, w.connected));
        if !exists && watches.len() >= inner.config.max_artifacts {
            return Armed::Refused(format!(
                "this process already watches its maximum of {} artifacts",
                inner.config.max_artifacts
            ));
        }
        let watch = watches.entry(slug.to_owned()).or_insert_with(|| {
            let task = tokio::spawn(run(Arc::downgrade(inner), slug.to_owned())).abort_handle();
            Watch {
                url: ids::viewer_url(slug),
                title: None,
                connected: false,
                problem: None,
                subscribers: Vec::new(),
                own: VecDeque::new(),
                comments_primed: false,
                pick_up_recent: false,
                comments_after: None,
                seen: HashMap::new(),
                comment_check: Arc::default(),
                task,
            }
        });
        if published && let Some(v) = version {
            remember_own(watch, v);
        }
        if explicit && !watch.comments_primed {
            watch.pick_up_recent = true;
        }
        // The newest watch of an artifact answers its comments.
        for s in &mut watch.subscribers {
            s.replies = false;
        }
        match watch.subscribers.iter_mut().find(|s| &*s.agent == agent) {
            Some(s) => {
                s.known = version.map(str::to_owned).or(s.known.take());
                s.replies = true;
                s.explicit |= explicit;
                s.since = Instant::now();
            }
            None => watch.subscribers.push(Subscriber {
                agent: agent.into(),
                known: version.map(str::to_owned),
                told: None,
                replies: true,
                explicit,
                since: Instant::now(),
            }),
        }
        if connected { Armed::Connected } else { Armed::Arming }
    }

    /// The agent read or published `version`: what it has now. A publish is
    /// also never news to the process's other agents.
    pub fn saw(&self, agent: &str, slug: &str, version: &str, published: bool) {
        let mut watches = self.inner.lock();
        let Some(watch) = watches.get_mut(slug) else { return };
        if published {
            remember_own(watch, version);
        }
        if let Some(s) = watch.subscribers.iter_mut().find(|s| &*s.agent == agent) {
            s.known = Some(version.to_owned());
        }
    }

    /// The artifact was published from this local file: its comments are
    /// handed to the session (it changes the source), not answered on their own.
    pub fn published_from(&self, slug: &str, path: &Path) {
        let mut sources = self.inner.sources.lock().unwrap_or_else(PoisonError::into_inner);
        if sources.len() >= 4096 {
            sources.clear();
        }
        sources.insert(slug.to_owned(), path.to_path_buf());
    }

    /// Stops `agent`'s watch of `slug`; false when it held none.
    pub fn unwatch(&self, agent: &str, slug: &str) -> bool {
        let mut watches = self.inner.lock();
        let held = watches.get(slug).is_some_and(|w| w.subscribers.iter().any(|s| &*s.agent == agent));
        if held {
            drop_subscriber(&mut watches, slug, agent);
        }
        held
    }

    /// A resumed session's watch: like an explicit one, its first read picks
    /// up comments sent to Claude in the last minutes (Claude Code's
    /// `pickUpRecentSummons` on resume).
    pub(crate) fn resumed(&self, slug: &str) {
        if let Some(watch) = self.inner.lock().get_mut(slug)
            && !watch.comments_primed
        {
            watch.pick_up_recent = true;
        }
    }

    /// One tool-less request through the host's model (`None`: none to ask).
    pub(crate) async fn compose(&self, agent: &str, request: &ComposeRequest) -> Option<Composed> {
        self.inner.notifier.compose(agent, request).await
    }

    /// Ties `agent` to its own record of its artifacts and to its sign-in
    /// (its extensions).
    pub(crate) fn attach(&self, agent: &str, extensions: &Extensions) {
        let state = Arc::downgrade(&extensions.get_or_default::<ArtifactState>());
        let mut states = self.inner.states.lock().unwrap_or_else(PoisonError::into_inner);
        if states.get(agent).is_some_and(|s| s.ptr_eq(&state)) {
            return;
        }
        let mut clients = self.inner.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if states.len() >= 4096 {
            states.retain(|_, s| s.strong_count() > 0);
            clients.retain(|agent, _| states.contains_key(agent));
        }
        states.insert(agent.to_owned(), state);
        let signed = self.inner.client.signed_for(extensions);
        if Arc::ptr_eq(&signed, &self.inner.client) {
            clients.remove(agent);
        } else {
            clients.insert(agent.to_owned(), signed);
        }
    }

    /// Ends every watch of an agent that has ended.
    pub fn forget(&self, agent: &str) {
        self.inner.states.lock().unwrap_or_else(PoisonError::into_inner).remove(agent);
        self.inner.clients.lock().unwrap_or_else(PoisonError::into_inner).remove(agent);
        let mut watches = self.inner.lock();
        let slugs: Vec<String> = watches
            .iter()
            .filter(|(_, w)| w.subscribers.iter().any(|s| &*s.agent == agent))
            .map(|(s, _)| s.clone())
            .collect();
        for slug in slugs {
            drop_subscriber(&mut watches, &slug, agent);
        }
    }

    pub fn list(&self, agent: &str) -> Vec<WatchRow> {
        let watches = self.inner.lock();
        let mut rows: Vec<WatchRow> = watches
            .values()
            .filter_map(|w| {
                let s = w.subscribers.iter().find(|s| &*s.agent == agent)?;
                Some(WatchRow {
                    url: w.url.clone(),
                    connected: w.connected,
                    problem: w.problem.clone(),
                    replies: s.replies,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.url.cmp(&b.url));
        rows
    }

    /// Artifacts with an open or pending connection.
    pub fn artifacts(&self) -> usize {
        self.inner.lock().len()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Watch>> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The client `slug`'s watch works through: the sign-in of the agent
    /// that answers its comments.
    fn client_for(&self, slug: &str) -> Arc<FrameClient> {
        let agent = self.lock().get(slug).and_then(|w| {
            let answers = w.subscribers.iter().find(|s| s.replies);
            answers.or_else(|| w.subscribers.iter().max_by_key(|s| s.since)).map(|s| Arc::clone(&s.agent))
        });
        let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        agent.and_then(|agent| clients.get(&*agent).cloned()).unwrap_or_else(|| Arc::clone(&self.client))
    }

    fn source_of(&self, slug: &str) -> Option<PathBuf> {
        self.sources.lock().unwrap_or_else(PoisonError::into_inner).get(slug).cloned()
    }

    /// An automatic edit `agent`'s session published: never news to the
    /// process's agents, and `agent` has it now, as after its own publish
    /// (Claude Code runs the edit as the session's own Artifact tool call,
    /// which records the version), so its next publish is not refused as
    /// built on an older one.
    fn own_version(&self, slug: &str, agent: &str, version: &str, title: Option<&str>) {
        if let Some(watch) = self.lock().get_mut(slug) {
            remember_own(watch, version);
            if let Some(s) = watch.subscribers.iter_mut().find(|s| &*s.agent == agent) {
                s.known = Some(version.to_owned());
            }
        }
        let state = self.states.lock().unwrap_or_else(PoisonError::into_inner).get(agent).and_then(Weak::upgrade);
        if let Some(state) = state {
            let title = title.map(str::to_owned).or_else(|| state.get(slug).and_then(|k| k.title));
            state.record(slug, Known { version: version.to_owned(), title, typed: false });
        }
    }

    /// True the first time for an artifact.
    fn mark_notified(&self, slug: &str) -> bool {
        self.notified.lock().unwrap_or_else(PoisonError::into_inner).insert(slug.to_owned())
    }
}

fn remember_own(watch: &mut Watch, version: &str) {
    if !watch.own.iter().any(|v| v == version) {
        watch.own.push_back(version.to_owned());
        while watch.own.len() > OWN_VERSIONS {
            watch.own.pop_front();
        }
    }
}

/// Removes `agent` from a watch; the last one out closes the connection.
fn drop_subscriber(watches: &mut HashMap<String, Watch>, slug: &str, agent: &str) {
    let Some(watch) = watches.get_mut(slug) else { return };
    watch.subscribers.retain(|s| &*s.agent != agent);
    if watch.subscribers.is_empty() {
        watch.task.abort();
        watches.remove(slug);
    } else if !watch.subscribers.iter().any(|s| s.replies)
        && let Some(next) = watch.subscribers.iter_mut().max_by_key(|s| s.since)
    {
        next.replies = true;
    }
}

/// Version ids are `<unix seconds>-<hash>`: `a` is news over `b` when it is
/// later, or as late but another version.
fn newer(a: &str, b: &str) -> bool {
    let secs = |v: &str| v.split('-').next().and_then(|n| n.parse::<u64>().ok());
    match (secs(a), secs(b)) {
        (Some(x), Some(y)) => x > y || (x == y && a != b),
        _ => a != b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order() {
        assert!(newer("1794124245-2ccf", "1794124234-1f2a"));
        assert!(!newer("1794124234-1f2a", "1794124245-2ccf"));
        assert!(newer("1794124234-aaaa", "1794124234-bbbb"));
        assert!(!newer("1794124234-aaaa", "1794124234-aaaa"));
    }
}
