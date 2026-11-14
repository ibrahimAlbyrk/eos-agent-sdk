#!/usr/bin/env bash
# Two tiny live turns on a GPT model through your ChatGPT plan: a plain
# reply, then a Read round trip (its encrypted reasoning goes back on the
# second request). Sign in first:
#   ~/.cargo/bin/cargo run -q --release -p eos-agent-cli -- login chatgpt
# Usage: scripts/openai-live-check.sh [model]   (default: gpt = the account's first listed model)
# Env: EOS_CHATGPT_STORE (default ~/.eos-agent/chatgpt), EOS_OPENAI_TOOLS (namespace|top|additional).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
model="${1:-gpt}"
store="${EOS_CHATGPT_STORE:-$HOME/.eos-agent/chatgpt}"
if [[ ! -f "$store/siwc.json" ]]; then
  echo "Not signed in to ChatGPT ($store). Run: ~/.cargo/bin/cargo run -q --release -p eos-agent-cli -- login chatgpt" >&2
  exit 1
fi
cargo build -q --release -p eos-agent-cli
bin="$PWD/target/release/eos-agent"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf 'The project codename is BLUEFIN.\n' > "$work/notes.txt"

echo "== 1/2 plain turn on $model"
"$bin" --model "$model" --no-mcp --effort low --cwd "$work" run "Reply with just the word ok."
echo
echo "== 2/2 tool turn on $model (Read, then the reasoning is sent back)"
"$bin" --model "$model" --no-mcp --effort low --read-only --cwd "$work" run \
  "Use the Read tool on notes.txt, then reply with just the codename."
echo
echo "live check passed"
