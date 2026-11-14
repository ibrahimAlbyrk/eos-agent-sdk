# Session API for a host that links the SDK

What a Rust host that owns one `Sdk` and many agents uses besides `spawn` and
`send_input`: the events it builds its transcript from, credentials per agent,
Claude sign-in, the side question, and continuing a Claude Code session.

## Events

An agent's events arrive on the channel `Sdk::spawn` returns
(`eos_agent_core::AgentEvent`); its subagents' on
`AgentOptions::subagent_events`; background tasks' on
`AgentOptions::task_events`. A host that stops reading pauses the agent.

### One response

```
RequestStarted { attempt }
  BlockStart { index, block }        block: Text | Thinking | ToolUse { id, name }
    TextDelta | ThinkingDelta |      | ServerToolUse { id, name } | Other(type)
    ToolInputDelta { id, partial_json }
  BlockStop { index }
  ToolUse { id, name, input }        right after the BlockStop of a tool call
Usage(ResponseUsage { usage, model })
Message(assistant message)
```

- Blocks never overlap, so a delta belongs to the block opened last. A tool
  call's id and name come with its `BlockStart`, before its input streams.
- `StreamRestarted`: the attempt broke mid-stream and is sent again; drop the
  blocks and deltas since the last `RequestStarted`.
- `Usage.model` is the model the API says answered (the requested one when
  the provider does not say).

### Tool calls

`ToolResult { id, output }` carries the result the model gets
(`output.content`, `output.is_error`) and, for the host only,
`output.details`: a JSON object in the shape of Claude Code's `toolUseResult`.

| Tool | `details` |
|---|---|
| Edit | `{filePath, oldString, newString, replaceAll, structuredPatch}` |
| Write | `{type: "create" \| "update", filePath, structuredPatch}` (no hunks for a new file) |

A `structuredPatch` hunk is `{oldStart, oldLines, newStart, newLines, lines}`,
three lines of context, each line prefixed with a space, `-` or `+`. The
file's old and new text are not copied into it.

### Turns and compaction

`TurnEnded { stop_reason, stated }` or `Interrupted` ends a turn that ran; a
turn that failed ends with the error `send_input` returns, and no event.
`Compacting { before_tokens }` says the conversation is being summarized;
`Compacted { before_tokens }` follows when the summary replaced it. A summary
that fails or is interrupted ends with the error or interruption of the turn
(or `compact` call) that ran it.

A request the provider refuses because the conversation no longer fits the
model's context window is `ModelError::ContextWindowExceeded` (Claude: the
`invalid_request_error` "prompt is too long: N tokens > M maximum", as an
HTTP answer or a stream's `error` event; OpenAI: `context_length_exceeded`).
The agent summarizes once and sends the request again; with
`auto_compact(false)` the turn fails with that error and the host summarizes
(`SdkAgent::compact`).

### Subagents

`SubagentEvent { agent_id, parent_id, event }`, where `event` is

- `Started(SubagentStarted { tool_use_id, agent_type, model, description, name, background })`:
  a turn of the subagent begins; `tool_use_id` is the parent's call that
  started it.
- `Agent(AgentEvent)`: one of its own events.
- `Ended(SubagentEnded { status, error, usage, tool_uses, duration_ms })`:
  that turn is over: `completed`, `killed` (stopped, or its caller stopped
  waiting) or `failed`; `usage` is its last response's.

A subagent that is woken later (SendMessage, or a note from its own background
work) sends another `Started` … `Ended`.

### Background tasks

`TaskEvent { agent_id, task_id, tool_use_id, kind }`. A workflow run sends
`Started`, `Progress` and `Ended` (see [protocol.md](protocol.md#taskevent) for their fields). A Bash command in the background
(asked for, or moved there after its timeout) and a Monitor are `local_bash`
tasks; `tool_use_id` is the call that started them:

- `Started { task_type: "local_bash", description, command, output_file, .. }`
- `Output { total_bytes }`: the output file (a monitor: its stream) has grown;
  about once a second while it grows. Read the file for the text.
- `Event { text }` (a Monitor only): what the monitor told its agent while it
  runs: a batch of new lines, or its own notice (lines dropped, a timeout).
  The lines that arrive with its exit come in the end's summary to the agent
  only.
- `Ended { status, summary, output_file, usage.duration_ms, exit_code, .. }`:
  `completed`, `failed` or `stopped`; `exit_code` when the command exited by
  itself.

The end (except a stop) and each Monitor event also reach the agent as a
`<task-notification>`, which wakes it when it is idle: the agent's events
then show a turn the host did not start.

## Credentials per agent

`SdkBuilder::auth` (Claude) and `SdkBuilder::openai` / `chatgpt_store` give
the credentials of agents that bring none. `AgentOptions::auth(source)` gives
one agent, and every subagent it starts, its own; it must be one for the
provider the agent's model runs on. The agent's WebFetch and WebSearch side
requests use it too.

`Account` is a ready source that remembers plan limits:

```rust
let account = Arc::new(Account::claude_store(&store_dir)); // or Account::api_key(key), Account::new(source)
let options = AgentOptions::new(cwd).auth(account.clone());
account.rate_limit();   // Option<RateLimitInfo>: status, resets_at, claim, 5-hour and 7-day utilization
```

Share one `Account` between all agents on the same sign-in. Renewal is
correct either way: every holder of a store renews under Claude Code's
`<store>/.oauth_refresh.lock` and re-reads the store once it holds the lock,
so a single-use refresh token is spent once.

### When a credential stops working

`SdkBuilder::auth_failover(Arc<dyn AuthFailover>)`: when a request is refused
because the plan's limit is reached (`AuthFailure::LimitReached(info)`, with
`info.resets_at`) or the sign-in is over (`AuthFailure::Expired(why)`), the
host is asked, while the request waits and before the turn fails, for
another credential for that agent:

```rust
fn replacement(&self, agent_id: &str, failure: &AuthFailure) -> BoxFuture<'_, Option<Arc<dyn TokenSource>>>;
```

- `Some(source)`: the whole tree moves to it and the same request goes out
  again; the turn carries on and nothing is added to the conversation.
- `None`: the turn fails with `ModelError::LimitReached { info }` or
  `ModelError::Auth(_)`. Later, `SdkAgent::set_auth(source)` (another sign-in,
  or an API key for this agent only) and `SdkAgent::resume()` run the cut-off
  turn again with no new input, as Claude Code re-runs an interrupted turn.
  `resume` answers `None` when the conversation does not end with a user
  message.

`set_auth` can be called at any time; it takes effect with the next request.

## Claude sign-in

`eos_agent_anthropic::auth` signs a store in the way `claude auth login`
does: authorization code with PKCE, the browser coming back to
`http://localhost:<port>/callback` or the user pasting the code the page
shows, the code exchanged at Claude's token endpoint, the profile read for
the plan.

```rust
let login = ClaudeLogin::keychain(Some(&store_dir));
let sign_in = SignIn::start(SignInConfig::default()).await?;
open_in_browser(sign_in.browser_url());       // comes back by itself
show_to_user(sign_in.manual_url());           // for a browser elsewhere: shows `code#state`
let account = sign_in.finish(&login, pasted_code).await?;   // pasted_code: Future<Output = Option<String>>
login.sign_out().await?;
```

`finish` uses whichever comes first, the browser or the pasted code, and
returns `ClaudeAccount { email, account_uuid, display_name, organization_uuid,
organization_name, subscription_type, rate_limit_tier, scopes }`. The store's
credentials document gets Claude Code's `claudeAiOauth` member
(`accessToken`, `refreshToken`, `expiresAt`, `scopes`, `subscriptionType`,
`rateLimitTier`), in the Keychain item Claude Code uses for that store
directory (its `.credentials.json` there when the Keychain cannot be
written); other members (MCP sign-ins) are kept. Sign-out removes the member.
`~/.claude.json` is not written.

From the terminal: `eos-agent login claude [--store <dir>] [--no-browser]`
(default store `~/.eos-agent/claude`), then `--claude-store <dir>`;
`eos-agent logout claude [--store <dir>]` signs the store out.

A ChatGPT store has the same call: `ChatGptLogin::new(&store_dir).sign_out()`
removes its sign-in record and leaves the other files in the directory. Both
sign-outs are local: the tokens are forgotten on this computer and not
revoked at the provider.

## Side question

`SdkAgent::ask_side(question)` is Claude Code's `/btw`: one reply from the
agent's conversation as it stands, with no tool run. The request is the
agent's own (system prompt, tools, messages, credential), so it reads the
agent's prompt cache, plus Claude Code's side-question reminder and the
question; nothing joins the conversation or the transcript. While a turn runs
it keeps going and the answer is drawn from what the turn last sent the
model. Dropping the returned future cancels the question. A reply with no
text is `SideError::NoAnswer`.

## Continuing a Claude Code session

`eos_agent_compat::ClaudeSession::read(path)` reads a transcript Claude Code
wrote (`transcript_path(&paths, cwd, session_id)` is
`<config>/projects/<project>/<session>.jsonl`) into the messages a resumed
session sends the model:

- the branch the session ended on, from its last compaction on;
- with the conversation input Claude Code delivered as attachments, in
  order, each run of it a user message of its own after the turn's tool
  results (as the engine records the notes it delivers): queued commands (a
  message sent while the model worked, a `<task-notification>`, a
  teammate's message) and teammate mailbox messages (as
  `<teammate-message>`);
- without other attachments (context a session here gets anew: CLAUDE.md,
  skills, tool and MCP deltas, plan mode, file snapshots), subagent
  sidechains, API-error notes and Claude Code's bookkeeping entries;
- one assistant message per response (Claude Code writes one entry per
  content block), neighbouring user entries joined;
- every tool call with its result: a call that never got one gets Claude
  Code's stand-in error result, a result whose call is gone is dropped;
- images and other blocks as they were written.

```rust
let session = ClaudeSession::read(&path)?;
let history = if same_model { session.messages } else { session.without_thinking() };
store.replace(&id, &history).await?;                       // so the engine's store holds it
let (agent, events) = sdk.spawn(AgentOptions::new(cwd).id(id).history(history)).await?;
agent.resume().await?;                                     // only if it was cut off mid-turn
```

`session.model` is the model of the last reply; thinking blocks are signed
for their model, so `without_thinking()` is for continuing on another.
