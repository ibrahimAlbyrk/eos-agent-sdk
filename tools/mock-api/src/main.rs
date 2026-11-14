//! Serves one recorded `.sse` body (or a fixed text reply) to every request,
//! for poking at clients by hand.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use eos_agent_mock_api::{MockApi, Reply};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7899")]
    addr: SocketAddr,
    /// Recorded SSE body to replay; without it the mock answers "Hello from the mock".
    #[arg(long)]
    sse: Option<PathBuf>,
    /// How many requests to answer before the script runs out.
    #[arg(long, default_value_t = 1000)]
    replies: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let reply = match &args.sse {
        Some(path) => Reply::sse(std::fs::read_to_string(path)?),
        None => Reply::text("Hello from the mock"),
    };
    let mock = MockApi::bind(args.addr, std::iter::repeat_n(reply, args.replies)).await?;
    println!("mock API at {}", mock.base_url());
    tokio::signal::ctrl_c().await?;
    Ok(())
}
