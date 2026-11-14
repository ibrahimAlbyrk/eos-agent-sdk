# Eos Agent SDK

A Rust engine for AI coding agents. An agent is a small piece of data inside one process, so one
program can run many of them side by side. It is the engine behind [Eos](https://github.com/ibrahimAlbyrk/eos).
macOS first.

## What it does

- Runs agent sessions against Claude models (Anthropic Messages API) and OpenAI models (Responses
  API on a ChatGPT plan).
- Gives each agent its tools: files, shell with a sandbox, web fetch and search, subagents,
  workflows, skills, MCP servers and artifacts.
- Lets a host program decide what an agent may do through one permission gate.
- Keeps each session's transcript, so a session can be resumed, rewound or forked.

## Use it

| How | Where |
|---|---|
| As a Rust library: link `eos-agent-sdk` and drive agents directly | [docs/host-api.md](docs/host-api.md), [docs/session-api.md](docs/session-api.md) |
| From another program over JSON-RPC (stdio or a unix socket): `eos-agent-server` | [docs/protocol.md](docs/protocol.md) |
| From a terminal: the `eos-agent` command | below |

```bash
cargo run --release -p eos-agent-cli -- login claude --store ~/.eos-agent/claude   # browser sign-in
cargo run --release -p eos-agent-cli -- --claude-store ~/.eos-agent/claude run "hi"

cargo run --release -p eos-agent-cli -- login chatgpt         # browser sign-in → ~/.eos-agent/chatgpt
cargo run --release -p eos-agent-cli -- --model gpt run "hi"  # the account's first listed model
cargo run --release -p eos-agent-cli -- logout chatgpt        # forget the sign-in on this computer
```

## Layout

| Path | What |
|---|---|
| `crates/eos-agent-types` | Wire types: content blocks, stream events, usage |
| `crates/eos-agent-core`, `crates/eos-agent-engine` | The agent loop, its conversation and the commands that drive it |
| `crates/eos-agent-anthropic`, `crates/eos-agent-openai` | Provider adapters: requests, streaming, sign-in, model catalog |
| `crates/eos-agent-oauth`, `crates/eos-agent-http` | Kits the adapters share: secret stores, token renewal, PKCE, SSE framing |
| `crates/eos-agent-tools-*`, `crates/eos-agent-agents`, `crates/eos-agent-workflow`, `crates/eos-agent-skills`, `crates/eos-agent-mcp`, `crates/eos-agent-artifacts` | The tools an agent can use |
| `crates/eos-agent-prompt` | Prompt assembly |
| `crates/eos-agent-sdk` | Composition: shared services once per process, everything above per agent |
| `crates/eos-agent-server`, `crates/eos-agent-cli` | The JSON-RPC host and the terminal command |
| `crates/eos-agent-testkit`, `tools/mock-api`, `tools/scale-bench` | Test support: a scripted model API, allocation and footprint measurement |
| `xtask` | `cargo xtask layers`: enforces the dependency direction between crates |

## Develop

```bash
scripts/check.sh               # fmt, clippy (-D warnings), tests, dependency direction
scripts/openai-live-check.sh   # two tiny live turns on a ChatGPT sign-in
```

Tests run against `tools/mock-api`; none of them calls a real model.
