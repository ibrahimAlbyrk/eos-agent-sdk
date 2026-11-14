//! One benchmark run: spawn, run rounds of turns, measure.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_engine::JsonlStore;
use eos_agent_sdk::{AgentOptions, Sdk, SdkAgent};
use eos_agent_testkit::{AllocStats, cpu_time, footprint_bytes, peak_footprint_bytes, slope};
use futures_util::future::join_all;

const PROMPT: &str = "Read small.txt, then say done.";
/// Turns run before measuring, to fill pools and caches.
const WARMUP_TURNS: usize = 3;

pub(crate) struct Plan {
    pub(crate) agents: usize,
    pub(crate) rounds: usize,
    pub(crate) evict: Option<Duration>,
    pub(crate) pause: Duration,
    pub(crate) recycle: Option<usize>,
}

/// A scratch tree: a home with no Claude Code config, a project with the
/// file every turn reads, and the transcripts.
pub(crate) struct Dirs {
    root: PathBuf,
}

impl Dirs {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let root = std::env::temp_dir().join(format!("eos-scale-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "project", "data", "transcripts"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        let root = root.canonicalize()?;
        std::fs::write(root.join("project/small.txt"), "Eos benchmark file.\nIt has three lines.\nThe end.\n")?;
        Ok(Self { root })
    }

    pub(crate) fn file(&self) -> PathBuf {
        self.root.join("project/small.txt")
    }

    fn transcripts(&self) -> PathBuf {
        self.root.join("transcripts")
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Bytes of every transcript on disk: the agents' whole histories.
fn history_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum()
}

pub(crate) struct Report {
    agents: usize,
    turns: usize,
    evict: Option<Duration>,
    recycle: Option<usize>,
    spawn_wall: Duration,
    spawn_cpu: Duration,
    spawn_allocs: u64,
    turns_wall: Duration,
    turns_cpu: Duration,
    turns_allocs: u64,
    footprint: [u64; 3],
    peak: u64,
    /// Heap per agent spawned, before its first turn.
    spawned: i64,
    /// Heap and footprint per agent once all are idle; then after eviction.
    idle: (i64, i64),
    evicted: Option<(i64, i64)>,
    history: u64,
    /// Per round: footprint, heap, heap minus the histories it holds.
    samples: [Vec<u64>; 3],
}

pub(crate) async fn run(plan: &Plan, dirs: &Dirs, url: &str) -> anyhow::Result<Report> {
    let mut builder = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("bench".into()))))
        .base_url(url)
        .http2_prior_knowledge(true)
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.root.join("home")))
        .data_dir(dirs.root.join("data"))
        .store(Arc::new(JsonlStore::new(dirs.transcripts())))
        .idle_eviction(plan.evict);
    builder = builder.artifacts(false);
    let sdk = builder.build().await?;
    let options = AgentOptions::new(dirs.root.join("project")).mcp(false);
    {
        let (warm, _) = sdk.spawn(options.clone()).await?;
        for _ in 0..WARMUP_TURNS {
            warm.send_text(PROMPT).await?.map_err(|e| anyhow::anyhow!("warmup turn: {e}"))?;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let footprint_before = footprint_bytes();
    let heap_before = AllocStats::now();
    let (cpu, started) = (cpu_time(), Instant::now());
    let mut agents: Vec<SdkAgent> = Vec::with_capacity(plan.agents);
    for _ in 0..plan.agents {
        // The event channel is dropped: nobody listens, nothing queues.
        agents.push(sdk.spawn(options.clone()).await?.0);
    }
    let (spawn_wall, spawn_cpu) = (started.elapsed(), cpu_time() - cpu);
    let spawn_allocs = AllocStats::now().allocs - heap_before.allocs;
    let footprint_spawned = footprint_bytes();
    let heap_spawned =
        (AllocStats::now().live_bytes as i64 - heap_before.live_bytes as i64) / plan.agents.max(1) as i64;

    let mut samples: [Vec<u64>; 3] = Default::default();
    let mut turns_wall = Duration::ZERO;
    let mut turns_cpu = Duration::ZERO;
    let mut turns_allocs = 0;
    for round in 0..plan.rounds {
        if plan.recycle.is_some_and(|every| round > 0 && round % every == 0) {
            let n = agents.len();
            agents.clear();
            for _ in 0..n {
                agents.push(sdk.spawn(options.clone()).await?.0);
            }
        }
        let (cpu, started, allocs) = (cpu_time(), Instant::now(), AllocStats::now().allocs);
        for result in join_all(agents.iter().map(|a| a.send_text(PROMPT))).await {
            result?.map_err(|e| anyhow::anyhow!("turn: {e}"))?;
        }
        turns_wall += started.elapsed();
        turns_cpu += cpu_time() - cpu;
        turns_allocs += AllocStats::now().allocs - allocs;
        tokio::time::sleep(plan.pause).await;
        let heap = AllocStats::now().live_bytes;
        let history =
            if plan.evict.is_some() || plan.recycle.is_some() { 0 } else { history_bytes(&dirs.transcripts()) };
        samples[0].push(footprint_bytes());
        samples[1].push(heap);
        samples[2].push(heap.saturating_sub(history));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let per_agent = |heap: u64, footprint: u64| {
        let n = plan.agents.max(1) as i64;
        ((heap as i64 - heap_before.live_bytes as i64) / n, (footprint as i64 - footprint_before as i64) / n)
    };
    let idle = per_agent(AllocStats::now().live_bytes, footprint_bytes());
    let evicted = match plan.evict {
        Some(after) => {
            tokio::time::sleep(after + Duration::from_millis(500)).await;
            Some(per_agent(AllocStats::now().live_bytes, footprint_bytes()))
        }
        None => None,
    };
    let report = Report {
        agents: plan.agents,
        turns: plan.agents * plan.rounds,
        evict: plan.evict,
        recycle: plan.recycle,
        spawn_wall,
        spawn_cpu,
        spawn_allocs,
        turns_wall,
        turns_cpu,
        turns_allocs,
        footprint: [footprint_before, footprint_spawned, footprint_bytes()],
        peak: peak_footprint_bytes(),
        spawned: heap_spawned,
        idle,
        evicted,
        history: history_bytes(&dirs.transcripts()),
        samples,
    };
    drop(agents);
    Ok(report)
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per = |d: Duration, n: usize| d.as_secs_f64() * 1e3 / n.max(1) as f64;
        let evict = self.evict.map_or_else(|| "off".to_owned(), |d| format!("after {} ms", d.as_millis()));
        let recycle = self.recycle.map_or_else(String::new, |n| format!(", fresh agents every {n} rounds"));
        writeln!(f, "{} agents, {} turns, eviction {evict}{recycle}", self.agents, self.turns)?;
        writeln!(
            f,
            "spawn: {:.2} s wall, {:.3} ms CPU and {} allocations per agent",
            self.spawn_wall.as_secs_f64(),
            per(self.spawn_cpu, self.agents),
            self.spawn_allocs / self.agents.max(1) as u64
        )?;
        writeln!(
            f,
            "turns: {:.2} s wall, {:.3} ms CPU and {} allocations per turn",
            self.turns_wall.as_secs_f64(),
            per(self.turns_cpu, self.turns),
            self.turns_allocs / self.turns.max(1) as u64
        )?;
        let [before, spawned, after] = self.footprint;
        writeln!(
            f,
            "footprint: {:.1} MB before, {:.1} MB spawned, {:.1} MB after, {:.1} MB peak",
            mb(before),
            mb(spawned),
            mb(after),
            mb(self.peak)
        )?;
        writeln!(f, "spawned agent: {} B heap", self.spawned)?;
        writeln!(f, "idle agent: {} B heap, {} B footprint", self.idle.0, self.idle.1)?;
        if let Some((heap, footprint)) = self.evicted {
            writeln!(f, "idle agent after eviction: {heap} B heap, {footprint} B footprint")?;
        }
        writeln!(
            f,
            "histories on disk: {:.1} MB ({} B per agent)",
            mb(self.history),
            self.history / self.agents.max(1) as u64
        )?;
        if self.samples[0].len() > 2 {
            let kb = |s: &[u64]| slope(s) / 1024.0;
            let late = |s: &[u64]| kb(&s[s.len() / 2..]);
            write!(
                f,
                "per round: footprint {:+.1} KB ({:+.1} KB over the second half), heap {:+.1} KB ({:+.1} KB)",
                kb(&self.samples[0]),
                late(&self.samples[0]),
                kb(&self.samples[1]),
                late(&self.samples[1])
            )?;
            if self.evict.is_none() && self.recycle.is_none() {
                write!(f, ", heap besides histories {:+.1} KB", kb(&self.samples[2]))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}
