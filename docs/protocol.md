# eos-agent-server protocol

`eos-agent-server` hosts Eos agents for another program. It speaks
[JSON-RPC 2.0](https://www.jsonrpc.org/specification), one JSON object per
line (UTF-8, `\n`-terminated, no line breaks inside a message), in both
directions:

- **stdio** (default): the client writes requests to the server's stdin and
  reads its stdout. Closing stdin ends the session; the server interrupts
  every agent and exits.
- **unix socket**: `eos-agent-server --socket <path>` listens on `<path>`
  (mode `0600`, so only the same user can connect). Every connection is its
  own session with its own `initialize`, agents and permission prompts.

The client calls the server (`initialize`, `agent.*`, `task.stop`, `complete`, `mcp.*`); the server sends
`agent.event` notifications and calls the client back (`permission.decide`,
`user.ask`, `mcp.authorize`, `mcp.stepUp`). Requests are handled concurrently: an `agent.send` that runs for
a minute does not hold up an `agent.interrupt` sent after it. Server request
ids are strings (`"s1"`, `"s2"`, …).

## Errors

| code | meaning |
|---|---|
| -32700 | the line is not JSON |
| -32600 | not a valid request; `initialize` called twice; setup failed |
| -32601 | no such method |
| -32602 | invalid params (also: a `/command` that cannot be expanded) |
| -32000 | the turn failed (model error after retries, transcript write failed) |
| -32001 | no such agent (never spawned, closed, or stopped) |
| -32002 | `initialize` has not been called |

## Methods (client → server)

### `initialize`

Must come first, once per session. Every field is optional.

```json
{
  "auth": {"claudeStore": "/Users/me/.eos/accounts/claude-x"},
  "baseUrl": "https://api.anthropic.com",
  "openaiBaseUrl": "https://api.openai.com/v1",
  "model": "claude-opus-5-5",
  "transcripts": "/path/to/transcripts",
  "dataDir": "/path/to/data",
  "idleEvictionSecs": 600,
  "permissions": "ask",
  "permissionTimeoutMs": 60000,
  "askTimeoutMs": 600000,
  "artifacts": true,
  "artifactsAuth": {"claudeStore": "/Users/me/.eos/accounts/claude-x"},
  "sandbox": {"enabled": true, "network": {"allowedDomains": ["github.com", "*.npmjs.org"]}},
  "workflows": true,
  "workflowDir": "/path/to/workflow-runs"
}
```

- `auth`: one of `{"claudeStore": dir}` (a Claude sign-in store, Claude
  Code's `CLAUDE_SECURESTORAGE_CONFIG_DIR`), `{"apiKeyEnv": "VAR"}` (an API
  key in the server's environment variable `VAR`), `{"authTokenEnv": "VAR"}`
  (an OAuth bearer token in `VAR`), `{"openaiStore": dir}` (a ChatGPT sign-in
  store, written by `eos-agent login chatgpt --chatgpt-store dir` or the
  SDK's `eos_agent_openai::sign_in`: OpenAI models on the user's ChatGPT
  plan), or a list of them, one per provider:
  `[{"claudeStore": …}, {"openaiStore": …}]`. Without it: `ANTHROPIC_API_KEY`
  / `ANTHROPIC_AUTH_TOKEN`, else the default Claude sign-in; with only an
  `openaiStore`, Claude models are unavailable. Credentials are never sent
  over the protocol itself.
- `openaiBaseUrl`: where OpenAI requests go (default
  `https://api.openai.com/v1`).
- `model` and `agent.spawn`'s `model` take OpenAI slugs (`gpt-*`, `o3`, …,
  or any slug written `openai/<slug>`) and `gpt` (the account's first listed
  model) once an `openaiStore` is set. An agent never changes provider: a
  command's `model:` on another provider is not used for the turn. Its
  subagents may run on another provider (an agent type's `model:`); one on a
  provider without a credential runs on the parent's model.
  MCP servers' OAuth sign-ins are kept in the same place: the Keychain item of
  `claudeStore`, else Claude Code's own credentials item, so a sign-in made
  in Claude Code works here and the other way round.
- `transcripts`: a folder for one JSONL transcript per agent. Needed for
  `resume` and for idle eviction.
- `idleEvictionSecs`: an agent idle this long keeps its history only in its
  transcript (paged back on its next turn, byte-for-byte the same); a finished
  subagent is let go (not while a command or monitor of its own still runs)
  and restarted from its transcript when messaged. `0` =
  never. Default 600. No effect without `transcripts`.
- `permissions`: `"ask"` (default: every tool call becomes a
  `permission.decide` request), `"allow"` (everything runs), `"readOnly"` (no
  file writes, only read-only shell commands; nothing is asked).
- `permissionTimeoutMs` (default 60 000): a `permission.decide` with no answer
  by then is a deny. `askTimeoutMs` (default 600 000): the same for `user.ask`.
- `artifacts`: the Artifact tools on or off (default: on when their
  credential is a Claude sign-in).
- `artifactsAuth`: the Artifact tools' own credential, in `auth`'s shape
  (default: `auth`'s). claude.ai's frame API takes only a Claude sign-in, so a
  model billed on an API key (`{"apiKeyEnv": …}`) can publish through
  `{"claudeStore": dir}` or `{"authTokenEnv": VAR}`. A sign-in that cannot be
  read leaves the tools off (unless `artifacts: true`); the model is not
  affected.
- `sandbox`: Claude Code's `sandbox` settings object, unchanged (camelCase,
  as in its `settings.json`), in the place of Claude Code's `--settings`:
  each agent's sandbox is the user's `~/.claude/settings.json`, its working
  directory's `.claude/settings.json` and `.claude/settings.local.json`,
  this, then the managed settings (set values win, lists are joined, with
  Claude Code's limits on what a repository's settings may widen). Off
  unless some tier sets `enabled: true` (Claude Code's default). While on (macOS), every Bash and
  Monitor command runs under its own Seatbelt profile: it may write only in
  the agent's working directories, its `$TMPDIR` and `filesystem.allowWrite`
  (minus Claude Code's protected files: its settings and state, `.claude`
  files up to the home folder, git hooks and config, shell rc files, …), read everywhere but
  `filesystem.denyRead`, and reach the network only through the server's
  filtering proxy. Keys: `enabled`, `failIfUnavailable` (with no sandbox on
  this machine, starting an agent fails instead of running unsandboxed),
  `autoAllowBashIfSandboxed` (default true), `allowUnsandboxedCommands`
  (default true), `excludedCommands`, `ignoreViolations`,
  `filesystem.{allowWrite, denyWrite, denyRead, allowRead}`,
  `filesystem.allowManagedReadPathsOnly`,
  `network.{allowedDomains, deniedDomains, strictAllowlist, allowUnixSockets,
  allowAllUnixSockets, allowLocalBinding, allowMachLookup,
  allowManagedDomainsOnly, tlsTerminate}`,
  `credentials.{files, envVars, allowPlaintextInject, awsPairs, sigv4}`,
  `allowAppleEvents`, `enableWeakerNetworkIsolation`. With
  `network.tlsTerminate` the proxy terminates the agent's TLS tunnels with
  its own CA (commands' trust variables point at it) and swaps masked
  `credentials.envVars` placeholders for their real values on the way out.
  Other keys (own proxy ports, Linux keys) are accepted and ignored.
- `workflows`: Claude Code's Workflow tool on or off for top-level agents
  (subagents never get it). Default as Claude Code decides: on, unless
  `CLAUDE_CODE_DISABLE_WORKFLOWS`, `CLAUDE_CODE_WORKFLOWS=0` or the
  `disableWorkflows` / `enableWorkflows` settings turn it off. Its size
  guideline comes from the `workflowSizeGuideline` setting (default
  `medium`).
- `workflowDir`: where workflow runs keep their files (default `dataDir`), in
  Claude Code's layout under `<workflowDir>/<agentId>/`:
  `workflows/scripts/<name>-<runId>.js` (the script, editable and passed back
  as `scriptPath`), `subagents/workflows/<runId>/journal.jsonl` (the
  `transcriptDir`: each agent's result, for resume), `workflows/<runId>.json`
  (the run record, written when it ends: Claude Code's fields, the final
  `workflowProgress` included) and `tasks/<taskId>.output`. Workflow agents'
  transcripts are in `transcripts`, by agent id, like any subagent's.

Result: `{"protocolVersion": 1, "server": {"name": "eos-agent-server", "version": "0.0.1"}}`

### `agent.spawn`

```json
{
  "cwd": "/path/to/project",
  "model": "sonnet",
  "effort": "high",
  "maxTokens": 32000,
  "contextWindow": 200000,
  "roots": ["/another/dir"],
  "id": "my-agent-1",
  "resume": false,
  "systemPrompt": "…",
  "appendSystemPrompt": "…",
  "allowedTools": ["Read", "Grep", "mcp__github__*"],
  "disallowedTools": ["Bash"],
  "mcp": true,
  "mcpServers": {"eos": {"type": "http", "url": "http://127.0.0.1:7401/mcp"}},
  "toolSearch": true,
  "autoCompact": true,
  "compactAt": 150000,
  "claudeMd": true,
  "pluginDirs": ["/path/to/a/plugin"],
  "thinkingSummaries": true,
  "interactive": false
}
```

Only `cwd` is required. `model` takes an id or an alias (`opus`, `sonnet`,
`haiku`, `fable`). `effort`, `maxTokens` and `contextWindow` default to what
Claude Code uses for the model. `thinkingSummaries: false` keeps thinking
hidden (no `thinkingDelta` events), as Claude Code's `-p` does. `systemPrompt` replaces Claude Code's system prompt,
`appendSystemPrompt` adds to it (not both). `allowedTools` lists the only
tools the agent and its subagents get; `disallowedTools` removes tools; a
trailing `*` matches by prefix. `mcp: false` skips the MCP servers configured
for `cwd`; otherwise they connect in the background as in Claude Code
(`agent.spawn` waits only for `alwaysLoad` servers, up to
`MCP_CONNECT_TIMEOUT_MS`, 5 s; every server with
`MCP_CONNECTION_NONBLOCKING=0`), and their tools reach the agent before the
request after they connect. `mcpServers` (Claude Code's `mcpServers` shape) names servers of the client's
choice: they connect beside the configured ones and win a name that repeats;
with `mcp: false` they are the agent's only servers. `toolSearch: false`
sends every tool in full, with no ToolSearch, to the agent and its
subagents. `autoCompact: false` leaves summarizing to `agent.compact` alone
(a request that no longer fits the window then fails the turn); `compactAt`
moves the automatic summary to that many context tokens. `claudeMd: false`
leaves the CLAUDE.md files out of the first message, for the agent and its
subagents. `pluginDirs` are plugin folders loaded as they are (Claude Code's
`--plugin-dir`): their skills and commands join the ones found for `cwd`.
`resume: true` with `id` continues that agent's recorded
conversation (needs `transcripts`). `interactive: true` behaves as Claude
Code's interactive terminal where it differs from its SDK sessions (the
default): a typed `/name` of a `context: fork` skill (`agent.send` with
`text`) launches its subagent in the background, so the turn ends at once
with the launch recorded, and the subagent's result arrives later as a task
notification that wakes the agent (its events arrive as usual).

Result: `{"agentId": "…"}`

### `agent.send`

```json
{"agentId": "…", "text": "/review the parser"}
{"agentId": "…", "content": [{"type": "text", "text": "…"}, {"type": "image", "source": {…}}]}
```

Exactly one of `text` (what a user typed: `/name args` naming a skill or
command is expanded first) or `content` (Messages API content blocks, sent as
they are). Runs one turn to its end, through every tool call and subagent.
A turn sent while another runs waits for it.

Result, after every event of the turn (its subagents' included) has been sent:
`{"outcome": "finished", "stopReason": "end_turn"}` or `{"outcome": "interrupted"}`.

### `agent.interrupt`

`{"agentId": "…"}` stops that agent's running turn at the next safe point.
Works for subagents too. Result: `{}`.

### `agent.notify`

`{"agentId": "…", "text": "…", "wake": false}` leaves a note for a
top-level agent's next request (a finished background job, a message from
elsewhere). With `wake: true` an idle agent starts a turn for it; its events
arrive as usual. Result: `{}`.

### `agent.compact`

`{"agentId": "…", "focus": "keep the API names"}` replaces the
conversation with a model-written summary now (`/compact`). Result:
`{"compacted": true}` (`false` when there was nothing to summarize).

### `agent.setModel`

`{"agentId": "…", "model": "haiku", "effort": "low"}` changes a top-level
agent's model, its effort, or both, from its next turn on (after any queued
turn); they hold until changed again, and subagents started later inherit
them. `model` takes an id or an alias and gets Claude Code's settings and
wording for that model, within what the agent was spawned with; a
conversation never changes provider (-32602). `effort: null` goes back to the
model's default; leaving `effort` out keeps it. A client that spawns the
agent again with `resume` passes the model itself.

Result: `{"model": "claude-haiku-5-5", "effort": "low"}` (what holds now).

### `agent.rewind`

`{"agentId": "…", "keep": 4}` cuts a top-level agent's conversation back to
its first `keep` messages (after any queued turn), in memory and in its
transcript, and goes on from there; `keep: 0` starts over with the session
context. Messages are counted as the transcript holds them: user messages,
assistant messages and the `role: "system"` notes. A cut that would keep a
tool call without its result, or `keep` past the end, is refused (-32602).
Files the dropped turns changed stay as they are. Result: `{}`.

### `agent.commands`

`{"agentId": "…"}` lists the skills and slash commands a top-level agent can
run, for a menu:

```json
[{"name": "review", "description": "Review a pull request", "argumentHint": "[pr number]",
  "source": "project", "plugin": null, "userInvocable": true, "modelInvocable": true}]
```

`source` is `bundled`, `user`, `project` or `plugin` (then `plugin` names
it). `userInvocable`: `agent.send` with `text: "/name …"` runs it.
`modelInvocable`: the model sees it in its skill listing.

### `task.stop`

`{"agentId": "…", "taskId": "b1x2y3z4w"}` stops one background task of a
top-level agent or of a subagent below it and waits until it has ended: a
background command or monitor, a workflow run (the `taskId` of its
`task.event`), or a working subagent (its agent id). The task's owner hears
of it as after its own TaskStop.

Result: `{"agentId": "…", "taskType": "local_bash", "description": "sleep 30"}`
(`taskType`: `local_bash`, `local_workflow` or `local_agent`; `agentId`: the
agent whose task it was). An unknown or already ended task is an error
(-32600) in TaskStop's words.

### `complete`

One request to a model outside any conversation: no tools, no agent, nothing
recorded; thinking is off where the model allows it (a model that always
thinks gets no thinking setting; see host-api.md, "One-shot requests").

```json
{"prompt": "Name this chat: …", "system": "You write short titles.", "model": "haiku",
 "effort": "low", "maxTokens": 200, "schema": {"type": "object", "properties": {…}}}
```

Only `prompt` is required. With `schema` (a JSON Schema) the model answers
through one `StructuredOutput` call (forced where the model takes that, else
asked for, and asked once more when missing) and the result's `structured`
is that object, checked against the schema (an answer that does not match
fails with -32000).

Result: `{"text": "…", "structured": null, "model": "claude-haiku-5-5",
"usage": {"input_tokens": 10, "output_tokens": 5}, "stopReason": "end_turn"}`

### `agent.list`

No params. Result: every live agent of the session, subagents included:

```json
[{"agentId": "…", "parentId": null, "name": null, "description": null, "agentType": null,
  "model": "claude-opus-5-5", "busy": false, "evicted": false}]
```

`parentId`, `name`, `description` and `agentType` are set for subagents. A
workflow's agents are subagents of the agent that launched the run
(`agentType` `"workflow-subagent"` or the script's `agentType`,
`description` the call's label).
`busy`: a turn is running or queued. `evicted`: the history (or, for a
finished subagent, the whole agent) is on disk only.

### `agent.close`

`{"agentId": "…"}` stops a top-level agent and every subagent it started,
and releases its MCP servers. Result: `{}`.

### `mcp.status`

`{"agentId": "…"}` lists the agent's MCP servers as Claude Code's `/mcp`
does. Result:

```json
[{"name": "docs", "transport": "http", "state": "needs-auth", "tools": 0, "error": null}]
```

`state`: `connected`, `pending` (still connecting in the background),
`needs-auth` (its tools are the two auth tools), `idle` (not connected right
now: closed while idle or not used yet) or `failed` (with `error`).
`transport` is `stdio`, `http` or `sse`.

### `mcp.reconnect`

`{"agentId": "…", "server": "docs"}` connects to the server again (`/mcp`
"Reconnect"). Result: `{}`.

### `mcp.signOut`

`{"agentId": "…", "server": "docs"}` is `/mcp`'s "Clear authentication":
the stored tokens are revoked at the authorization server's
`revocation_endpoint` (RFC 7009, refresh token then access token), the
sign-in is removed from the credentials store and the server is connected
again, now waiting for a new sign-in. Result: `{"signedOut": true}`
(`false` for a server without OAuth).

## Notifications (server → client)

### `agent.event`

```json
{"agentId": "a3f…", "parentId": "9c1…", "event": {"type": "textDelta", "text": "Hel"}}
```

`parentId` is present only for subagents (the agent that started it, a
top-level agent or another subagent). Events of one agent arrive in order;
events of different agents may interleave. Every event of a turn is sent
before that turn's `agent.send` / `agent.compact` response.

`event` is one of:

| `type` | fields | meaning |
|---|---|---|
| `requestStarted` | `attempt` | a model request goes out (1 = first try) |
| `streamRestarted` | | the previous attempt broke mid-stream: drop its partial deltas |
| `textDelta` | `text` | streamed reply text |
| `thinkingDelta` | `text` | streamed (summarized) thinking |
| `toolInputDelta` | `toolUseId`, `partialJson` | a tool's input while the model writes it |
| `toolUse` | `toolUseId`, `name`, `input` | a complete tool call (`input` is the JSON object) |
| `toolResult` | `toolUseId`, `content`, `isError`, `details?` | its result: a string or an array of content blocks. `details` is what the call did, for the client only, in the shape of Claude Code's `toolUseResult`: Edit gives `{filePath, oldString, newString, replaceAll, structuredPatch}`, Write `{type: "create" \| "update", filePath, structuredPatch}`; each `structuredPatch` hunk is `{oldStart, oldLines, newStart, newLines, lines}` with lines prefixed ` `, `-` or `+` |
| `message` | `message` | a message appended to the conversation, exactly as the API receives it: `{"role": "user" \| "assistant" \| "system", "content": [...]}` |
| `usage` | `usage`, `model` | one response's token usage as the API reports it (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, …) and the model that answered |
| `blockStart` | `index`, `block`, `toolUseId?`, `name?` | a content block opens (`text`, `thinking`, `tool_use`, `server_tool_use` or its wire type); blocks never overlap, so the deltas that follow belong to it. A tool call carries its id and name here, before its input streams |
| `blockStop` | `index` | the block is complete (a tool call's `toolUse` follows) |
| `rateLimit` | `status`, `resetsAt`, `claim`, `fiveHourUtilization`, `sevenDayUtilization` | plan-limit state (any may be `null`) |
| `retrying` | `attempt`, `delayMs`, `error` | a failed request is retried after `delayMs` |
| `turnEnded` | `stopReason` | the turn is over (`end_turn`, `max_tokens`, `refusal`, …) |
| `interrupted` | | the turn stopped on `agent.interrupt` |
| `subagentStarted` | `toolUseId`, `agentType`, `model`, `effort?`, `description`, `name?`, `background` | subagents only (`parentId` set): a turn of the subagent begins; `toolUseId` is the parent's call that started it |
| `subagentEnded` | `status`, `error?`, `usage`, `toolUses`, `durationMs` | subagents only: that turn is over (`completed`, `killed` or `failed`), with the usage of its last response |
| `compacting` | `beforeTokens` | the conversation is being summarized; `compacted` follows on success |
| `compacted` | `beforeTokens` | the conversation was replaced by its summary |

A `system` message that adds tools mid-conversation (Claude Code's
tool-change betas) carries `tool_addition` / `tool_removal` blocks after its
text. Late tools come with their definitions (the default) unless
`CLAUDE_CODE_INLINE_TOOLS=0` in the server's environment sends them by
reference, as in Claude Code. One block shape is the transcript's own and never reaches
the API: `{"type": "tool_addition", "tool": {"type": "same_as_earlier",
"name": "…"}}` stands for the definition that tool had when it was last
withdrawn (Claude Code's `sameAs` pointer); requests carry the definition
itself. Hand such messages back unchanged when resuming (`history`).

Field names of the envelope and of these events are camelCase; objects the
API defines (`message`, `usage`, content blocks, tool inputs) keep the API's
own snake_case shape. New event types may be added; clients should ignore
types they do not know.

### `task.event`

A background task of an agent (a workflow run, a backgrounded Bash command,
a Monitor) started, made progress or ended. These mirror Claude Code's SDK
messages `task_started`, `task_progress` and `task_notification`:

```json
{"agentId": "9c1…", "taskId": "w4k2…", "toolUseId": "toolu_…", "event": {"type": "started", "taskType": "local_workflow", "description": "Review changed files", "workflowName": "review-changes", "runId": "wf_1a2b3c4d-5e6", "transcriptDir": "/…/9c1…/subagents/workflows/wf_1a2b3c4d-5e6"}}
{"agentId": "9c1…", "taskId": "w4k2…", "toolUseId": "toolu_…", "event": {"type": "progress", "description": "Review: review:bugs", "usage": {"total_tokens": 1234, "tool_uses": 7, "duration_ms": 4100}, "lastToolName": "review:bugs", "summary": "Review changed files", "workflowProgress": [{"type": "workflow_phase", "index": 1, "title": "Review"}, {"type": "workflow_agent", "index": 1, "label": "review:bugs", "phaseIndex": 1, "phaseTitle": "Review", "agentId": "a3f…", "model": "claude-opus-5-5", "state": "progress", "queuedAt": 1762600000000, "startedAt": 1762600000100, "lastProgressAt": 1762600004100, "lastToolName": "Grep", "lastToolSummary": "TODO", "promptPreview": "…", "tokens": 1234, "toolCalls": 7}]}}
{"agentId": "9c1…", "taskId": "w4k2…", "toolUseId": "toolu_…", "event": {"type": "ended", "status": "completed", "summary": "Dynamic workflow \"Review changed files\" completed", "outputFile": "/…/tasks/w4k2….output", "usage": {"total_tokens": 9876, "tool_uses": 42, "duration_ms": 61000}, "workflowProgress": […]}}
```

A shell task (`taskType` `local_bash`: a Bash command run in the background
or moved there after its timeout, or a Monitor script; `toolUseId` names the
call that started it) sends:

```json
{"agentId": "9c1…", "taskId": "b3k9x0a1z", "toolUseId": "toolu_…", "event": {"type": "started", "taskType": "local_bash", "description": "Run the dev server", "command": "npm run dev", "outputFile": "/…/tasks/b3k9x0a1z.output"}}
{"agentId": "9c1…", "taskId": "b3k9x0a1z", "toolUseId": "toolu_…", "event": {"type": "output", "totalBytes": 2048}}
{"agentId": "9c1…", "taskId": "b3k9x0a1z", "toolUseId": "toolu_…", "event": {"type": "event", "text": "build failed: 2 errors"}}
{"agentId": "9c1…", "taskId": "b3k9x0a1z", "toolUseId": "toolu_…", "event": {"type": "ended", "status": "failed", "summary": "Background command \"Run the dev server\" failed with exit code 1", "outputFile": "/…/tasks/b3k9x0a1z.output", "usage": {"total_tokens": 0, "tool_uses": 0, "duration_ms": 5120}, "exitCode": 1}}
```

`output` says the task's output file has grown to `totalBytes` (about once a
second while it grows); `event` (a Monitor only) is what the monitor told its
agent: a batch of new lines, or its own notice (lines dropped for coming too
fast, a timeout); `status` is `completed`, `failed` or `stopped`, and
`exitCode` is present when the command exited by itself.

For a workflow run:

- `started` comes when the Workflow tool launched the run (its result
  carries the same `runId` and `transcriptDir`).
- `progress` comes at most every 16 ms while rows change.
  `workflowProgress`, when present, is the run's whole progress list in
  Claude Code's own row shape (what its SDK sends as
  `system/task_progress.workflow_progress`): `workflow_phase` rows
  (`index`, `title`, `kind`) and `workflow_agent` rows (`index`, `label`,
  `phaseIndex`, `phaseTitle`, `agentId`, `agentType`, `isolation`, `model`,
  `state` `start` | `progress` | `done` | `error`, `queuedAt`, `startedAt`,
  `lastProgressAt`, `durationMs`, `attempt`, `lastAttemptReason`,
  `lastToolName`, `lastToolSummary`, `promptPreview`, `resultPreview`,
  `tokens`, `toolCalls`, `error`, `cached`, `skipped`, `endedWithRun`).
  Rows merge by `(type, index)`; `start` without `startedAt` is a queued
  agent. Log lines are not in it (they are in the run record), and a batch
  that only moves agents' tool ticks leaves it out unless 10 s passed.
- `ended`: `status` is `completed`, `failed` or `stopped` (TaskStop). Agents
  still open are settled as errors that `endedWithRun`. A completed or failed
  run also sends its `<task-notification>` to the agent, which wakes it.

Each agent's own events keep arriving as `agent.event` with `parentId` set
to the launching agent. TaskStop with the run's `taskId` stops a run.

## Requests (server → client)

The client answers with a normal JSON-RPC response carrying the same `id`.

### `permission.decide`

Sent before every tool call when `permissions` is `"ask"`, reads outside
the working directories included (this is their only check; writes outside
them are refused whatever the answer). Read's `file_path` and Glob's and
Grep's `path` come absolute:

```json
{"agentId": "…", "toolUseId": "toolu_…", "tool": "Write", "input": {"file_path": "/x", "content": "…"}, "sandboxed": false,
 "rootAgentId": "…", "parentAgentId": "…", "parentToolUseId": "toolu_…", "subagent": true, "workflow": false}
```

`rootAgentId` is the top-level agent of the caller's tree (the caller itself
when no agent started it). For a subagent (`subagent: true`), `parentAgentId`
is the agent that started it and `parentToolUseId` that agent's tool call
(absent for a typed command's fork); a top-level agent's requests carry
neither. `workflow` is true for a workflow run's agents and the agents they
start.

`sandboxed` is true for a Bash or Monitor call that will run inside the
sandbox. With the sandbox on and `autoAllowBashIfSandboxed` (the default), a
sandboxed Bash or Monitor call is allowed without this request, as in Claude Code,
except the calls Claude Code still asks about: a variable assignment off its
safe list (`DYLD_INSERT_LIBRARIES=…`), a redirect to `/dev/tcp` or
`/dev/udp`, `cd` together with `rm`, or an `rm`/`rmdir` of a system
directory, a working directory or one of its parents, or a target it cannot
resolve. Calls with `dangerouslyDisableSandbox: true` and excluded commands
come with `sandboxed: false`.

A sandboxed command connecting to a host that neither `allowedDomains` nor
`deniedDomains` decides (and with no `strictAllowlist`) is put to the client
as a call of the synthetic tool `SandboxNetworkAccess`; the connection waits
for the answer:

```json
{"agentId": "…", "toolUseId": "net_0", "tool": "SandboxNetworkAccess", "input": {"host": "pypi.org", "port": 443}, "sandboxed": false}
```

One question per host at a time; an allow holds for that host (any port) for
the rest of the top-level agent's session, its subagents included; a deny is
not remembered. With `permissions: "allow"` every host is allowed; with
`"readOnly"` none is asked about and sandboxed commands are not auto-allowed.

Answer `{"decision": "allow"}` or `{"decision": "deny", "message": "…"}`. An
allow may carry `"updatedInput": {…}`: the tool then runs with that input
instead of the model's (Claude Code's `updatedInput`; the conversation keeps
the call as the model wrote it). The
message goes back to the model as the tool's error (default: Claude Code's
"The user doesn't want to proceed with this tool use…"). An error response, an
unreadable answer or no answer within `permissionTimeoutMs` denies. Subagents
ask too; their `agentId` is the subagent's (its `toolUse` event, with
`parentId`, arrives first).

### `user.ask`

AskUserQuestion, for top-level agents (subagents never get the tool):

```json
{"agentId": "…", "questions": [{"question": "Which database?", "header": "DB", "multiSelect": false,
  "options": [{"label": "Postgres", "description": "…"}, {"label": "SQLite", "description": "…"}]}]}
```

Answer with the chosen labels (or free text) by question:

```json
{"answers": {"Which database?": "Postgres"},
 "annotations": {"Which database?": {"notes": "keep it local"}},
 "response": null}
```

`response` is free text the user wrote instead of answering. `{"declined":
true, "reason": "…"}` tells the model nobody will answer. `{"cancelled":
true}` is Claude Code's Esc: the call is rejected ("…STOP what you are doing
and wait for the user to tell you how to proceed.") and the agent's turn ends
as interrupted. No answer within `askTimeoutMs` tells the model to proceed on
its own judgment.

### `mcp.authorize`

A remote MCP server needs the user to sign in (OAuth). Sent when an agent
calls the server's `mcp__<server>__authenticate` tool:

```json
{"server": "github", "url": "https://github.com/login/oauth/authorize?response_type=code&…",
 "redirectUri": "http://localhost:53412/callback", "agentId": "…"}
```

Show the user `url` (open it in their browser). After they approve, the
browser lands on `redirectUri`, served by the server on 127.0.0.1, and the
sign-in completes on its own; the server's tools then replace its
`authenticate` / `complete_authentication` tools before the agent's next
request. If the browser cannot reach that address (another machine), the user
pastes the address it shows to the agent, which passes it to
`mcp__<server>__complete_authentication`. The sign-in waits five minutes. The
answer is not used: reply `{}` (or `{"opened": true}`) right away. The same
address also reaches the model in the `authenticate` tool's result.

### `mcp.stepUp`

A top-level agent's MCP tool call was refused because the sign-in lacks a
scope (403 `insufficient_scope`). As Claude Code asks in the call, the server
asks the client, only with `permissions: "ask"` (otherwise, and for
subagents, the call fails and the server waits for a sign-in, as before):

```json
{"server": "github", "scope": "repo:write",
 "question": "MCP server \"github\" needs additional permissions for this tool (it asked for the scope \"repo:write\"). Re-authenticate now?",
 "choices": [
   {"value": "reauthenticate", "label": "Re-authenticate in browser", "description": "Signs this server out until you finish authorizing in the browser (for the scope above plus what it already has); if you do not finish, run /mcp to sign back in. The tool call is retried once afterwards"},
   {"value": "not_now", "label": "Not now", "description": "The tool call fails with the permission error; other tools keep working"}],
 "agentId": "…"}
```

Answer `{"choice": "reauthenticate"}` or `{"choice": "not_now"}`. On
`reauthenticate` the sign-in starts at once: an `mcp.authorize` request
follows with the address (the scope asked for is the server's plus what the
token had); once the browser comes back, the server is connected again and
the call retried once; its result starts with `Re-authenticated MCP server
"github" (scope: "repo:write")`. If that retry is refused for the same scope,
the call fails and that scope is not asked about again in this process. Any
other answer, an error or no answer within `askTimeoutMs` is "not now".
Questions for one server are not repeated while one is open: every call
waiting on it gets its outcome.

## Example

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"auth":{"apiKeyEnv":"ANTHROPIC_API_KEY"},"permissions":"ask"}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"server":{"name":"eos-agent-server","version":"0.0.1"}}}
→ {"jsonrpc":"2.0","id":2,"method":"agent.spawn","params":{"cwd":"/repo","model":"haiku"}}
← {"jsonrpc":"2.0","id":2,"result":{"agentId":"5d0c…"}}
→ {"jsonrpc":"2.0","id":3,"method":"agent.send","params":{"agentId":"5d0c…","text":"create notes.txt"}}
← {"jsonrpc":"2.0","method":"agent.event","params":{"agentId":"5d0c…","event":{"type":"requestStarted","attempt":1}}}
← {"jsonrpc":"2.0","method":"agent.event","params":{"agentId":"5d0c…","event":{"type":"toolUse","toolUseId":"toolu_1","name":"Write","input":{"file_path":"/repo/notes.txt","content":"…"}}}}
← {"jsonrpc":"2.0","id":"s1","method":"permission.decide","params":{"agentId":"5d0c…","toolUseId":"toolu_1","tool":"Write","input":{…}}}
→ {"jsonrpc":"2.0","id":"s1","result":{"decision":"allow"}}
← {"jsonrpc":"2.0","method":"agent.event","params":{"agentId":"5d0c…","event":{"type":"toolResult","toolUseId":"toolu_1","content":"File created successfully at: /repo/notes.txt","isError":false}}}
← … textDelta, message, usage …
← {"jsonrpc":"2.0","method":"agent.event","params":{"agentId":"5d0c…","event":{"type":"turnEnded","stopReason":"end_turn"}}}
← {"jsonrpc":"2.0","id":3,"result":{"outcome":"finished","stopReason":"end_turn"}}
```
