# Hosting agents from Rust

A Rust program links `eos-agent-sdk`, builds one `Sdk` and drives many agents
through it directly. This page lists what a host can give an agent and do
with it; [session-api.md](session-api.md) covers the events, credentials per
agent, Claude sign-in, the side question and Claude Code transcripts;
`docs/protocol.md` is the JSON-RPC face, for programs that use
`eos-agent-server` instead.

```rust
let sdk = Sdk::builder().auth(credential).permissions(gate).store(store).build().await?;
let (agent, events) = sdk
    .spawn(
        AgentOptions::new(cwd)
            .model("sonnet")
            .tools(host_tools)                 // the host's own Tool implementations
            .extensions(session)               // what those tools find in ToolContext::extensions
            .system_prompt(SystemPromptOverride::Append(rules))
            .mcp(false)
            .mcp_servers(servers),             // only these MCP servers
    )
    .await?;
agent.send_input("/review the parser").await?;
```

## The host's own tools

`AgentOptions::tools(Vec<HostTool>)` adds tools the host implements
(`eos_agent_core::Tool`) to one agent, beside the built-in ones.

- **Same pipeline as built-in tools.** Every call goes to the permission
  gate, runs with the agent's `ToolContext`, shows up as `ToolUse` and
  `ToolResult` events, and a text result over 100,000 characters is saved to
  `<scratch>/tool-results/<tool use id>.txt` with Claude Code's
  `<persisted-output>` preview in its place. What a tool returns for the
  host alone (`ToolOutput::with_details`, a JSON object) reaches the host
  with the call's `ToolResult` event, spilled result or not. Descriptions
  have no length cap.
- **Names.** Any name works. A tool named `mcp__<server>__<tool>` is ordered
  and searched like an MCP tool (after the built-in ones, by name). A host
  tool named like a built-in one takes its place. `allowed_tools` and
  `disallowed_tools` apply to host tools too.
- **Tool search.** The tool's `ToolSpec` decides: as it is, the tool is sent
  in full with every request (an MCP server's `alwaysLoad`); marked
  `.deferred()` it waits behind ToolSearch, found by its name, description
  and `.with_search_hint("…")`.
- **Subagents** get host tools by the rule for every tool: an agent type
  that takes all tools, or names the tool, has it. A tool marked
  `HostTool::main_agent_only()` stays with the agent it was given to:
  subagents start without it, and a fork (which keeps its parent's tool list,
  to share its prompt cache) is refused with "<name> is only available to the
  main agent." before the host's gate is asked.
- **Session state.** `AgentOptions::extensions(Extensions)` seeds the
  `Extensions` of the agent and of every subagent and fork below it with the
  host's values (one per type, shared, not copied), so a tool reads the
  host's session (`ctx.extensions.get::<MySession>()`) instead of a global
  map. The seed is freed when the agent is dropped.
- **Sharing.** Agents spawned with the same tool `Arc`s (and the same
  filter, model wording and MCP tools) share one serialized tool list.

## Spawn options

| Option | What it does |
|---|---|
| `mcp(bool)` | Connect the MCP servers configured for the directory (default on). |
| `mcp_servers(Vec<McpServerConfig>)` | The host's own servers (Claude Code's `--mcp-config`): beside the configured ones, winning a name that repeats; with `mcp(false)` the agent's only servers (`--strict-mcp-config`). |
| `tool_search(false)` | Every tool sent in full, no ToolSearch, for the agent and its subagents. |
| `auto_compact(false)` | The conversation is summarized only when the host calls `SdkAgent::compact`; a request that no longer fits the window then fails the turn with `ModelError::ContextWindowExceeded`. For subagents too. |
| `compact_at(tokens)` | Summarize at this many context tokens instead of Claude Code's threshold for the model. Subagents on the same model inherit it; one on another model uses Claude Code's threshold for that model. |
| `compaction_text(Arc<dyn CompactionText>)` | The host's own summarizer wording: the instruction sent to the model and the message that replaces the conversation. For subagents too. |
| `claude_md(false)` | CLAUDE.md files stay out of the first message, for the agent and its subagents. |
| `plugin_dirs(Vec<PathBuf>)` | Plugin folders loaded as they are (`--plugin-dir`): their skills and commands join the directory's. |
| `system_prompt(Append(text) \| Replace(text))` | After Claude Code's system prompt, or instead of it. |
| `allowed_tools`, `disallowed_tools` | The only tools the agent and its subagents get; tools taken away. A trailing `*` matches by prefix. |

## The permission gate

`SdkBuilder::permissions(Arc<dyn PermissionGate>)` decides every tool call of
every agent. A `ToolCall` carries the agent id, the tool-use id, the tool's
name and input, whether it runs sandboxed, the agent's workspace, the paths
the call would write (`writes`) and its `origin`:

- `origin: None`: a top-level agent. `ToolCall::root_id()` is then the agent
  itself.
- `origin: Some(AgentOrigin { root_id, parent_id, tool_use_id, workflow })`:
  a subagent. `root_id` is the top-level agent of its tree, `parent_id` the
  agent that started it, `tool_use_id` the parent's Agent or Workflow call
  (none for a typed command's fork), `workflow` the run and `agent()` call
  it belongs to (also for agents a workflow's agent starts);
  `ToolCall::in_workflow()` tells. The synthetic requests
  (`SandboxNetworkAccess`, artifact reactions) carry the origin too.

The answer is `Permission::Allow`, `Permission::Deny(message)` (the message
goes back to the model as the tool's error) or
`Permission::AllowWith(input)`: the tool runs with that input instead of the
model's (Claude Code's `updatedInput`); the conversation keeps the call as
the model wrote it.

The input the gate sees is the model's with paths made absolute where the
tool says so (`Tool::observable_input`, Claude Code's
`backfillObservableInput`): Read's, Edit's and Write's `file_path`,
NotebookEdit's `notebook_path`, Glob's and Grep's `path`, Artifact's and
ArtifactData's `out_dir` (`~` expanded, relative paths from the agent's
current directory).

The gate decides reads and writes alike, inside the agent's working
directories or not. The tools that read (Read, Glob, Grep, Artifact files to
publish) take any path the process can read; the tools that write take any
path the process can write. `ToolCall::writes` lists what a call would create
or change (`Tool::written_paths`): Edit's and Write's `file_path`,
NotebookEdit's `notebook_path`, the folder an Artifact `read` with `path` or
`paths` saves into (`out_dir`), and `<out_dir>/<collection>` for an
ArtifactData `get`, `list` or `query` (the agent's scratch folder, the
default, is left out: saving there is the tool's own). Each `WriteTarget`
has the absolute `path` and `inside`: whether it lands inside the agent's
working directories (or one of the engine's files it may write, a session's
workflow scripts), judged by the path as written and by where its symlinks
lead, so a link inside the roots to a folder outside them is outside.
`ToolCall::writes_outside()` tells whether any lands outside. A host whose
modes accept edits without asking should accept only those with
`writes_outside()` false and ask about the rest; the bundled `ReadOnly` gate refuses Artifact reads that
save outside. A shell command's writes are not listed (the sandbox, when
on, bounds them).

Two rules hold whatever the gate answers, and a call that breaks one is
refused before the gate is asked:

- An agent isolated in its own worktree (a subagent started with
  `isolation: "worktree"`, a workflow `agent()` with `isolation:
  'worktree'`) writes only inside its working directories and the engine's
  writable files, so agents working in parallel cannot write over each
  other's files: `<path> is outside this agent's working directories.` The
  plain subagents it starts are held the same way; a plain subagent of an
  agent that is not isolated is not. `Workspace::is_confined()` tells.
- No tool writes a protected path outside the agent's working directories:
  the data dir, the workflow folder, and whatever the host adds with
  `SdkBuilder::protect_from_writes(path)` (its own data, sign-in stores):
  `<path> is in the agent engine's own files, which tools do not write.`
  The engine's writable files there (a session's workflow scripts) stay
  writable.

Read-before-write and the other checks of the write tools apply outside the
working directories as inside them.

## Changing a live agent

| Call | What it does |
|---|---|
| `SdkAgent::set_model(model)` | Runs the agent on another model of the same provider from its next turn on, with Claude Code's settings and wording for it, within what the agent was spawned with (its credential included). Holds until changed again, through history eviction; subagents started later inherit it. A host that spawns the agent again passes the model itself (`SdkAgent::model()` reads it). |
| `SdkAgent::set_effort(Some("low") \| None)` | The same for effort; `None` is the model's default. Holds through a later model change. |
| `SdkAgent::transcript()` | The conversation as it is now. |
| `SdkAgent::rewind(keep)` | Cuts the conversation back to its first `keep` messages, in memory and in the transcript store (the JSONL store keeps the old file beside the new one). `keep: 0` (or a `keep` that leaves only the host's seed) starts over with the session context. A cut may not keep a tool call without its result. What tools remember of the dropped messages (files read) and files they changed stay. |
| `SdkAgent::replace_context(seed)` | Replaces the conversation with the host's own summary of it, recorded at once and running no turn (Claude Code's seed pushed with `shouldQuery: false`). It is the host's context, not a prompt: the transcript store marks it `isMeta` and `StoredMessage::is_meta` reads it. The next input opens the conversation after it, with the session context; a rewind to that input keeps the seed. |
| `SdkAgent::fork_at(keep, options)` | Starts another agent from the first `keep` messages; this one goes on unchanged. |
| `SdkAgent::stop_task(task_id)`, `Sdk::stop_task(agent_id, task_id)` | Stops one background command or monitor or workflow run (by id), or a subagent (by agent id or name) that works or waits on its own work, in the agent's tree, for the user, and waits until it has ended: what TaskStop does, except that the parent is told the user stopped it and another agent's message no longer resumes it. Errors read as TaskStop's. |
| `AgentHandle::extensions()` | The agent's own state for its tools, reachable while a turn runs. |
| `AgentHandle::wake()` | Runs a turn on the notes the agent's own background work left (its `Wake` asked for them), once it is idle and only if no request read them first. Background commands and Monitors call it through the agent's `Wake`; a host rarely needs it. |

## One-shot requests and the command list

`Sdk::complete(CompleteRequest::new(prompt).model("haiku").system(text))`
asks a model once outside any conversation: no tools, no agent, nothing
recorded, transient failures retried. Thinking is off where the model allows
it, as Claude Code turns it off: `disabled`, with an effort above `high`
lowered to `high` on a model that takes `disabled` only up to there (Opus 5);
no thinking setting on a model that always thinks (Opus 5.5, Sonnet 5.5,
Haiku 5.5, Fable, Mythos, and any Claude model the engine does not know),
which then thinks as it decides. On a Claude model the system prompt follows
Claude Code's identity line. With `.schema(json_schema)` the model answers
through one `StructuredOutput` call and `Completion::structured` is that
object, checked against the schema: the call is forced where the model takes
that; a model that always thinks takes no forced call, so it is asked for it
and, when its answer lacks it, asked once more with Claude Code's reminder.
`Completion::usage` is the answering request's usage; `.cancel(token)` (or dropping the
future) ends it. `.auth(source)` runs it on a credential of the host's choice
(the type `AgentOptions::auth` takes: an `Account`, any `TokenSource`) instead
of the SDK's own; a credential that fails ends the request with the model's
error, and nobody is asked for a replacement.

`SdkAgent::commands()` lists the skills and slash commands an agent can run
(name, description, argument hint, where it comes from, whether a user may
type it and whether the model sees it), in Claude Code's order of
precedence.
