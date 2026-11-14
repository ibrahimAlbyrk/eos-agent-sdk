//! Scale, soak and leak benchmarks of the SDK against the mock API.
//!
//! The mock runs in a child process (`scale-bench serve`), so this process's
//! footprint, heap and CPU time are the agents' alone. Every turn is one Read
//! of a small file and one short answer: two model requests and a tool call.
//!
//! ```text
//! cargo run --release -p eos-agent-scale-bench -- a              # 1,000 agents × 5 turns
//! cargo run --release -p eos-agent-scale-bench -- b              # 10,000 agents × 1 turn
//! cargo run --release -p eos-agent-scale-bench -- c              # 100 agents × 100 turns
//! cargo run --release -p eos-agent-scale-bench -- a --evict-ms 500
//! cargo run --release -p eos-agent-scale-bench -- c --recycle 10  # leak soak
//! ```

mod bench;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context;
use clap::{Parser, Subcommand};
use eos_agent_mock_api::{MockApi, read_then_answer};
use eos_agent_testkit::CountingAlloc;
use tokio::io::AsyncBufReadExt;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    scenario: Scenario,
    /// Agents idle this long page their history out (needs the store, always on here).
    #[arg(long, global = true)]
    evict_ms: Option<u64>,
    /// Pause between rounds (lets eviction happen during a soak).
    #[arg(long, global = true, default_value_t = 0)]
    pause_ms: u64,
    /// Replace every agent with a fresh one after this many rounds, so
    /// conversations stop growing and only a leak could.
    #[arg(long, global = true)]
    recycle: Option<usize>,
}

#[derive(Subcommand)]
enum Scenario {
    /// 1,000 agents, 5 turns each, all agents at once.
    A,
    /// 10,000 agents spawned, 1 turn each, all at once.
    B,
    /// A 10,000-turn soak: 100 agents, 100 rounds of one turn each.
    C,
    /// Any shape.
    Custom {
        #[arg(long)]
        agents: usize,
        #[arg(long)]
        rounds: usize,
    },
    /// The mock API (started by the benchmark in its own process).
    Serve {
        #[arg(long)]
        file: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    eos_agent_testkit::raise_fd_limit();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let (agents, rounds) = match args.scenario {
        Scenario::Serve { file } => return runtime.block_on(serve(file)),
        Scenario::A => (1_000, 5),
        Scenario::B => (10_000, 1),
        Scenario::C => (100, 100),
        Scenario::Custom { agents, rounds } => (agents, rounds),
    };
    let plan = bench::Plan {
        agents,
        rounds,
        evict: args.evict_ms.map(std::time::Duration::from_millis),
        pause: std::time::Duration::from_millis(args.pause_ms),
        recycle: args.recycle,
    };
    runtime.block_on(async {
        let dirs = bench::Dirs::new()?;
        let mut mock = start_mock(&dirs.file()).await?;
        let report = bench::run(&plan, &dirs, &mock.1).await;
        mock.0.kill().await.ok();
        println!("{}", report?);
        Ok(())
    })
}

/// Starts `scale-bench serve` and returns it with the URL it listens on.
async fn start_mock(file: &std::path::Path) -> anyhow::Result<(tokio::process::Child, String)> {
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("serve")
        .arg("--file")
        .arg(file)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting the mock")?;
    let stdout = child.stdout.take().context("mock stdout")?;
    let line = tokio::io::BufReader::new(stdout).lines().next_line().await?.context("mock said nothing")?;
    Ok((child, line.trim().to_owned()))
}

async fn serve(file: PathBuf) -> anyhow::Result<()> {
    let file = file.to_str().context("file path is not UTF-8")?.to_owned();
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), read_then_answer(&file)).await?;
    println!("{}", mock.base_url());
    // Ends when the benchmark kills it.
    std::future::pending::<()>().await;
    Ok(())
}
