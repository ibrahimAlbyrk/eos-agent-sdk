//! `eos-agent run "<prompt>"` answers once (as Claude Code's `-p`: nobody is
//! asked anything); `eos-agent chat` keeps a conversation (as Claude Code's
//! interactive terminal: questions are asked on the terminal, and a typed
//! `context: fork` skill launches in the background);
//! `eos-agent login chatgpt` signs in to a ChatGPT plan for OpenAI models;
//! `eos-agent login claude [--store <dir>]` signs a store in to a Claude plan;
//! `eos-agent logout chatgpt|claude [--store <dir>]` forgets a store's sign-in.

mod console;
mod dialog;
mod terminal;

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use eos_agent_anthropic::auth::{self, ClaudeLogin, Credential, SignIn, StaticCredential, TokenSource};
use eos_agent_core::{AgentEvent, TurnOutcome};
use eos_agent_engine::JsonlStore;
use eos_agent_openai::{ChatGptLogin, ModelCatalog, OpenAiClient, PrintOpener, SignInConfig, ToolPlacement};
use eos_agent_sdk::{
    AgentOptions, PrintPrompter, ReadOnly, SandboxConfig, Sdk, SdkAgent, SubagentEvent, SubagentEventKind, TaskEvent,
    TaskEventKind,
};
use eos_agent_types::{ToolResultContent, Usage};
use tokio::io::AsyncBufReadExt;

use crate::console::Stream;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "eos-agent", version, about = "A lightweight Claude (and GPT) agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    opts: Opts,
}

#[derive(Subcommand)]
enum Command {
    /// Answer one prompt and exit (after any background agents report).
    Run { prompt: String },
    /// Talk turn by turn; an empty line or Ctrl-D ends it. `/name args` runs a skill
    /// (a `context: fork` one in the background).
    Chat,
    /// Sign in through the browser (`chatgpt`: OpenAI models on your ChatGPT
    /// plan; `claude`: a Claude plan, into a sign-in store of its own).
    Login {
        #[arg(value_enum)]
        provider: LoginProvider,
        /// Print the sign-in address without opening a browser.
        #[arg(long)]
        no_browser: bool,
        /// The sign-in store to write (default ~/.eos-agent/claude or
        /// ~/.eos-agent/chatgpt); a sign-in replaces the one it holds. Use it
        /// later with --claude-store or --chatgpt-store.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },
    /// Forget a store's sign-in on this computer (nothing is revoked at the provider).
    Logout {
        #[arg(value_enum)]
        provider: LoginProvider,
        /// The sign-in store (default ~/.eos-agent/claude or ~/.eos-agent/chatgpt).
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LoginProvider {
    Chatgpt,
    Claude,
}

#[derive(Args)]
struct Opts {
    #[arg(long, global = true, default_value = "claude-opus-5-5")]
    model: String,
    /// low, medium, high, xhigh or max.
    #[arg(long, global = true)]
    effort: Option<String>,
    /// The model's output limit by default.
    #[arg(long, global = true)]
    max_tokens: Option<u32>,
    /// Working directory (default: the current one).
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Deny file writes and shell commands that are not read-only.
    #[arg(long, global = true)]
    read_only: bool,
    /// Do not connect the configured MCP servers.
    #[arg(long, global = true)]
    no_mcp: bool,
    /// Claude sign-in store dir (Claude Code's CLAUDE_SECURESTORAGE_CONFIG_DIR).
    /// Without it: ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN, else the default sign-in.
    #[arg(long, global = true, env = "CLAUDE_SECURESTORAGE_CONFIG_DIR")]
    claude_store: Option<PathBuf>,
    /// Bill the model on the API key in this environment variable, even with a sign-in store set.
    #[arg(long, global = true, value_name = "VAR")]
    api_key_env: Option<String>,
    /// Claude sign-in store for the artifact tools (default: the sign-in store
    /// when the model runs on --api-key-env, else the model's credential).
    #[arg(long, global = true, value_name = "DIR")]
    artifacts_claude_store: Option<PathBuf>,
    #[arg(long, global = true, env = "ANTHROPIC_BASE_URL")]
    base_url: Option<String>,
    /// ChatGPT sign-in store (written by `eos-agent login chatgpt`; default ~/.eos-agent/chatgpt).
    /// Used for OpenAI models (`--model gpt`, `gpt-*`, `openai/<slug>`) and subagents on them.
    #[arg(long, global = true, env = "EOS_CHATGPT_STORE", value_name = "DIR")]
    chatgpt_store: Option<PathBuf>,
    /// Where OpenAI requests go (default https://api.openai.com/v1).
    #[arg(long, global = true, env = "EOS_OPENAI_BASE_URL", value_name = "URL")]
    openai_base_url: Option<String>,
    /// How tools are placed in OpenAI requests: namespace (default), top or additional.
    #[arg(long, global = true, env = "EOS_OPENAI_TOOLS", value_name = "PLACEMENT")]
    openai_tools: Option<String>,
    /// Keep transcripts here (one JSONL per agent).
    #[arg(long, global = true)]
    transcripts: Option<PathBuf>,
    /// Run shell commands in the sandbox (Claude Code's `sandbox.enabled`).
    #[arg(long, global = true)]
    sandbox: bool,
    /// Claude Code's `sandbox` settings as JSON, or a file holding them.
    #[arg(long, global = true, value_name = "JSON|FILE")]
    sandbox_config: Option<String>,
    /// A host sandboxed commands may reach without asking (repeatable; `*.x.com`, `host:port`).
    #[arg(long = "allow-domain", global = true, value_name = "HOST")]
    allow_domains: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Login { provider, no_browser, store } => {
            let store = sign_in_store(*provider, store.as_ref(), &cli.opts);
            return match provider {
                LoginProvider::Chatgpt => login_chatgpt(&cli.opts, &store, !no_browser).await,
                LoginProvider::Claude => login_claude(&store, !no_browser).await,
            };
        }
        Command::Logout { provider, store } => {
            return logout(*provider, &sign_in_store(*provider, store.as_ref(), &cli.opts)).await;
        }
        Command::Run { .. } | Command::Chat => {}
    }
    let interactive = matches!(cli.command, Command::Chat);
    let cwd = match &cli.opts.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    let sdk = build(&cli.opts, interactive, &cwd).await?;
    let mut options = agent_options(&cli.opts, cwd, interactive);
    let tally = Arc::new(Mutex::new(Tally::default()));
    let (subagent_events, subagents) = mpsc::channel(64);
    options.subagent_events = Some(subagent_events);
    let (task_events, tasks) = mpsc::channel(64);
    options.task_events = Some(task_events);
    tokio::spawn(print_tasks(tasks));
    let (agent, events) = sdk.spawn(options).await.context("starting the agent")?;
    let printer = tokio::spawn(print_events(events, Arc::clone(&tally)));
    tokio::spawn(count_subagents(subagents, Arc::clone(&tally)));
    match cli.command {
        Command::Run { prompt } => {
            turn(&agent, &prompt).await?;
            agent.settle().await;
        }
        Command::Login { .. } | Command::Logout { .. } => {}
        Command::Chat if std::io::stdin().is_terminal() => {
            // The terminal's one owner reads the line, so a question asked
            // meanwhile takes the keyboard instead of racing for it.
            while let Some(line) = console::read_line().await {
                if line.trim().is_empty() {
                    break;
                }
                turn(&agent, &line).await?;
            }
        }
        Command::Chat => {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            loop {
                console::print(Stream::Err, "\n> ");
                let Some(line) = lines.next_line().await? else { break };
                if line.trim().is_empty() {
                    break;
                }
                turn(&agent, &line).await?;
            }
        }
    }
    drop(agent);
    let _ = printer.await;
    let total = tally.lock().unwrap_or_else(PoisonError::into_inner).total();
    console::println(Stream::Err, &format!("[usage] total: {total}"));
    Ok(())
}

/// Token counts of the responses: input, output, cache write, cache read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tokens {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

impl Tokens {
    fn add(&mut self, usage: &Usage) {
        self.input += usage.input_tokens.unwrap_or(0);
        self.output += usage.output_tokens.unwrap_or(0);
        self.cache_write += usage.cache_creation_input_tokens.unwrap_or(0);
        self.cache_read += usage.cache_read_input_tokens.unwrap_or(0);
    }

    fn plus(self, other: Self) -> Self {
        Self {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_write: self.cache_write + other.cache_write,
            cache_read: self.cache_read + other.cache_read,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} in, {} out, {} cache write, {} cache read",
            grouped(self.input),
            grouped(self.output),
            grouped(self.cache_write),
            grouped(self.cache_read)
        )
    }
}

/// `1234567` → `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What the agent and its subagents used, this turn and in all.
#[derive(Debug, Default)]
struct Tally {
    turn: Tokens,
    turn_subagents: Tokens,
    agent: Tokens,
    subagents: Tokens,
}

impl Tally {
    /// The turn's line, and a fresh turn.
    fn end_turn(&mut self) -> Summary {
        let summary = Summary { own: self.turn, subagents: self.turn_subagents };
        self.agent = self.agent.plus(self.turn);
        self.subagents = self.subagents.plus(self.turn_subagents);
        (self.turn, self.turn_subagents) = (Tokens::default(), Tokens::default());
        summary
    }

    fn total(&self) -> Summary {
        Summary { own: self.agent.plus(self.turn), subagents: self.subagents.plus(self.turn_subagents) }
    }
}

#[derive(Debug, Clone, Copy)]
struct Summary {
    own: Tokens,
    subagents: Tokens,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.own)?;
        if !self.subagents.is_empty() {
            write!(f, "; subagents: {}", self.subagents)?;
        }
        Ok(())
    }
}

/// A background task's start and end on stderr (workflow runs).
async fn print_tasks(mut events: mpsc::Receiver<TaskEvent>) {
    while let Some(event) = events.recv().await {
        match event.kind {
            TaskEventKind::Started { task_type, description, run_id, .. } => {
                let run = run_id.unwrap_or_default();
                console::println(
                    Stream::Err,
                    &format!("[task {}] {task_type} started: {description} ({run})", event.task_id),
                );
            }
            TaskEventKind::Ended { summary, usage, .. } => {
                console::println(
                    Stream::Err,
                    &format!(
                        "[task {}] {summary} ({} tokens, {} tool uses, {} ms)",
                        event.task_id, usage.total_tokens, usage.tool_uses, usage.duration_ms
                    ),
                );
            }
            TaskEventKind::Progress { .. } | TaskEventKind::Output { .. } | TaskEventKind::Event { .. } => {}
        }
    }
}

async fn count_subagents(mut events: mpsc::Receiver<SubagentEvent>, tally: Arc<Mutex<Tally>>) {
    while let Some(SubagentEvent { event, .. }) = events.recv().await {
        if let SubagentEventKind::Agent(AgentEvent::Usage(usage)) = event {
            tally.lock().unwrap_or_else(PoisonError::into_inner).turn_subagents.add(&usage.usage);
        }
    }
}

/// Where the model's credential comes from.
#[derive(Debug, PartialEq, Eq)]
enum ModelAuth {
    ApiKeyEnv(String),
    Store(PathBuf),
    /// ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN, else the default sign-in.
    Env,
}

/// The model's credential, and a sign-in store for the artifact tools when
/// they do not share it: the frame API takes only a sign-in, so a model on
/// an API key keeps publishing through the store that was set.
fn credentials(opts: &Opts) -> (ModelAuth, Option<PathBuf>) {
    let model = match (&opts.api_key_env, &opts.claude_store) {
        (Some(var), _) => ModelAuth::ApiKeyEnv(var.clone()),
        (None, Some(store)) => ModelAuth::Store(store.clone()),
        (None, None) => ModelAuth::Env,
    };
    let on_key = matches!(model, ModelAuth::ApiKeyEnv(_));
    let artifacts = opts.artifacts_claude_store.clone().or_else(|| opts.claude_store.clone().filter(|_| on_key));
    (model, artifacts)
}

/// The ChatGPT sign-in store: `--chatgpt-store`, else `~/.eos-agent/chatgpt`.
fn chatgpt_store(opts: &Opts) -> PathBuf {
    opts.chatgpt_store.clone().unwrap_or_else(|| {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from).join(".eos-agent").join("chatgpt")
    })
}

/// The store `login` and `logout` work on: `--store`, else the provider's own.
fn sign_in_store(provider: LoginProvider, named: Option<&PathBuf>, opts: &Opts) -> PathBuf {
    if let Some(store) = named {
        return store.clone();
    }
    match provider {
        LoginProvider::Chatgpt => chatgpt_store(opts),
        // Never the store agents run on unless it is named: a sign-in replaces what a store holds.
        LoginProvider::Claude => {
            std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from).join(".eos-agent").join("claude")
        }
    }
}

/// `eos-agent logout <provider>`: the store's sign-in is removed from this computer.
async fn logout(provider: LoginProvider, store: &std::path::Path) -> anyhow::Result<()> {
    let (name, signed_out) = match provider {
        LoginProvider::Chatgpt => ("ChatGPT", ChatGptLogin::new(store).sign_out().await),
        LoginProvider::Claude => ("Claude", ClaudeLogin::keychain(Some(store)).sign_out().await),
    };
    if signed_out.with_context(|| format!("signing out of {name}"))? {
        eprintln!("Signed out of {name} (store: {}).", store.display());
    } else {
        eprintln!("Not signed in to {name} (store: {}).", store.display());
    }
    Ok(())
}

/// `eos-agent login claude`: Claude's browser sign-in into `store`, as
/// `claude auth login` does it (a code shown in the browser can be pasted).
async fn login_claude(store: &std::path::Path, open_browser: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(store).await?;
    tokio::fs::set_permissions(store, std::fs::Permissions::from_mode(0o700)).await?;
    let login = ClaudeLogin::keychain(Some(store));
    let sign_in = SignIn::start(auth::SignInConfig::default()).await.context("starting the sign-in")?;
    eprintln!(
        "\nSign in to Claude in your browser. If it does not open, visit:\n{}\n\nIf the browser then shows a code, \
         paste it here and press Enter.\n",
        sign_in.manual_url()
    );
    if open_browser {
        let _ = std::process::Command::new("open")
            .arg(sign_in.browser_url())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    // A thread of its own: it may still wait for a line when the browser has answered.
    let (line, pasted) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let read = std::io::stdin().read_line(&mut text).is_ok_and(|n| n > 0 && !text.trim().is_empty());
        let _ = line.send(read.then_some(text));
    });
    let account =
        sign_in.finish(&login, async { pasted.await.ok().flatten() }).await.context("signing in to Claude")?;
    let who = account.email.as_deref().unwrap_or("your account");
    let plan = account.subscription_type.as_deref().map(|p| format!(" ({p} plan)")).unwrap_or_default();
    eprintln!("Signed in to Claude as {who}{plan}. Use it with: --claude-store {}", store.display());
    Ok(())
}

/// `eos-agent login chatgpt`: the browser sign-in, then the models the account offers.
async fn login_chatgpt(opts: &Opts, store: &std::path::Path, open_browser: bool) -> anyhow::Result<()> {
    let account = eos_agent_openai::sign_in(store, &SignInConfig::default(), &PrintOpener { open_browser })
        .await
        .context("signing in to ChatGPT")?;
    let who = account.email.as_deref().unwrap_or(&account.subject);
    eprintln!("Signed in to ChatGPT as {who} (store: {}).", store.display());
    if !account.plan_usage {
        anyhow::bail!(
            "ChatGPT plan use was not granted to this app. Run `eos-agent login chatgpt` again and allow it."
        );
    }
    let auth = Arc::new(ChatGptLogin::new(store));
    let client = Arc::new(OpenAiClient::new(auth, openai_config(opts)?)?);
    let catalog = ModelCatalog::new(client, Some(store)).ensure().await;
    let listed: Vec<String> = catalog
        .models
        .iter()
        .filter(|m| m.listed)
        .map(|m| match &m.display_name {
            Some(name) => format!("{} ({name})", m.slug),
            None => m.slug.clone(),
        })
        .collect();
    if listed.is_empty() {
        eprintln!("Could not list the account's models yet; `--model gpt-5.5` (or another slug) still works.");
    } else {
        eprintln!("Models (`--model gpt` is the first): {}", listed.join(", "));
    }
    Ok(())
}

fn openai_config(opts: &Opts) -> anyhow::Result<eos_agent_openai::ClientConfig> {
    let mut config = eos_agent_openai::ClientConfig::default();
    if let Some(url) = &opts.openai_base_url {
        config.base_url = url.trim_end_matches('/').to_owned();
    }
    if let Some(name) = &opts.openai_tools {
        config.tools =
            ToolPlacement::parse(name).with_context(|| format!("--openai-tools: unknown placement {name}"))?;
    }
    Ok(config)
}

const ASK_USER_QUESTION: &str = "AskUserQuestion";

fn agent_options(opts: &Opts, cwd: PathBuf, interactive: bool) -> AgentOptions {
    // Thinking is never printed, so it is not streamed either (as Claude Code's `-p`).
    let mut options =
        AgentOptions::new(cwd).model(&opts.model).mcp(!opts.no_mcp).thinking_summaries(false).interactive(interactive);
    options.max_tokens = opts.max_tokens;
    options.effort.clone_from(&opts.effort);
    if !interactive {
        // Claude Code's `-p` has nobody to ask, so it leaves the tool out.
        options.disallowed_tools.push(ASK_USER_QUESTION.to_owned());
    }
    options
}

/// `interactive`: questions (AskUserQuestion, an MCP server's step-up) are
/// asked on the terminal, as in Claude Code's interactive terminal; else
/// nobody is asked, as in its `-p`.
async fn build(opts: &Opts, interactive: bool, cwd: &std::path::Path) -> anyhow::Result<Sdk> {
    let (model, artifacts) = credentials(opts);
    let store = chatgpt_store(opts);
    let on_openai = eos_agent_openai::is_openai_model(&opts.model);
    if on_openai && !ChatGptLogin::exists(&store) {
        anyhow::bail!("{} runs on your ChatGPT plan: sign in first with `eos-agent login chatgpt`", opts.model);
    }
    let auth: Option<Arc<dyn TokenSource>> = match model {
        ModelAuth::ApiKeyEnv(var) => {
            let key = std::env::var(&var).ok().filter(|k| !k.trim().is_empty());
            let key = key.with_context(|| format!("--api-key-env: environment variable {var} is not set"))?;
            Some(Arc::new(StaticCredential(Credential::ApiKey(key))))
        }
        ModelAuth::Store(store) => Some(Arc::new(ClaudeLogin::keychain(Some(&store)))),
        ModelAuth::Env => match auth::from_env() {
            Some(auth) => Some(auth),
            // On an OpenAI model, Claude joins only when this Mac is signed in to it.
            None if on_openai && !claude_signed_in().await => None,
            None => Some(Arc::new(ClaudeLogin::keychain(None))),
        },
    };
    let mut builder = Sdk::builder().model(&opts.model);
    if interactive {
        builder = builder
            .prompter(Arc::new(terminal::TerminalUser::new(cwd)))
            .mcp_prompter(Arc::new(terminal::TerminalAuth::new(cwd)));
    } else {
        builder = builder.mcp_prompter(Arc::new(PrintPrompter::default().without_questions()));
    }
    if let Some(auth) = auth {
        builder = builder.auth(auth);
    }
    if ChatGptLogin::exists(&store) {
        let config = openai_config(opts)?;
        builder = builder.chatgpt_store(&store).openai_base_url(config.base_url).openai_tool_placement(config.tools);
    }
    if let Some(store) = &opts.claude_store {
        builder = builder.mcp_credentials(eos_agent_sdk::claude_mcp_credentials(Some(store)));
    }
    if let Some(store) = artifacts {
        builder = builder.artifact_auth(Arc::new(ClaudeLogin::keychain(Some(&store))));
    }
    if let Some(url) = &opts.base_url {
        builder = builder.base_url(url);
    }
    if let Some(dir) = &opts.transcripts {
        builder = builder.store(Arc::new(JsonlStore::new(dir)));
    }
    if opts.read_only {
        builder = builder.permissions(Arc::new(ReadOnly));
    }
    if let Some(config) = sandbox_config(opts)? {
        builder = builder.sandbox(config);
    }
    builder.build().await.context("setting up")
}

/// Whether Claude Code's default sign-in exists on this Mac.
async fn claude_signed_in() -> bool {
    use auth::SecretStore as _;
    matches!(auth::KeychainSecrets::for_claude_store(None).read().await, Ok(Some(_)))
}

/// The sandbox settings the flags give, over Claude Code's own.
fn sandbox_config(opts: &Opts) -> anyhow::Result<Option<SandboxConfig>> {
    let mut config = match &opts.sandbox_config {
        Some(arg) => {
            let text = if arg.trim_start().starts_with('{') {
                arg.clone()
            } else {
                std::fs::read_to_string(arg).with_context(|| format!("--sandbox-config: reading {arg}"))?
            };
            let value: serde_json::Value = serde_json::from_str(&text).context("--sandbox-config: not JSON")?;
            // A whole settings file or just its `sandbox` object.
            let value = value.get("sandbox").cloned().unwrap_or(value);
            Some(serde_json::from_value::<SandboxConfig>(value).context("--sandbox-config")?)
        }
        None => None,
    };
    if opts.sandbox {
        config.get_or_insert_default().enabled = Some(true);
    }
    if !opts.allow_domains.is_empty() {
        let c = config.get_or_insert_default();
        c.network.allowed_domains.get_or_insert_default().extend(opts.allow_domains.iter().cloned());
    }
    Ok(config)
}

async fn turn(agent: &SdkAgent, input: &str) -> anyhow::Result<()> {
    match agent.send_input(input).await? {
        Ok(TurnOutcome::Finished(_)) => Ok(()),
        Ok(TurnOutcome::Interrupted) => {
            console::println(Stream::Err, "\n[interrupted]");
            Ok(())
        }
        Err(e) => Err(e).context("the turn failed"),
    }
}

async fn print_events(mut events: mpsc::Receiver<AgentEvent>, tally: Arc<Mutex<Tally>>) {
    let err = |text: String| console::println(Stream::Err, &text);
    let end_turn = || {
        let summary = tally.lock().unwrap_or_else(PoisonError::into_inner).end_turn();
        err(format!("[usage] turn: {summary}"));
    };
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Usage(usage) => tally.lock().unwrap_or_else(PoisonError::into_inner).turn.add(&usage.usage),
            AgentEvent::TextDelta { text } => console::print(Stream::Out, &text),
            AgentEvent::ToolUse { name, input, .. } => err(format!("\n· {name}({})", summary(input.get()))),
            AgentEvent::ToolResult { output, .. } if output.is_error => {
                let text = match &output.content {
                    ToolResultContent::Text(t) => t.as_str(),
                    ToolResultContent::Blocks(_) => "",
                };
                err(format!("  ↳ error: {}", short(text.lines().next().unwrap_or(""), 120)));
            }
            AgentEvent::Retrying { attempt, delay, error } => {
                err(format!("\n[retry {attempt} in {:.1}s] {error}", delay.as_secs_f64()));
            }
            AgentEvent::StreamRestarted => err("\n[stream restarted]".into()),
            AgentEvent::TurnEnded { .. } => {
                console::print(Stream::Out, "\n");
                end_turn();
            }
            AgentEvent::Interrupted => end_turn(),
            AgentEvent::RateLimit(info) => err(format!("\n[plan limit] {info:?}")),
            AgentEvent::Compacted { before_tokens } => err(format!("\n[summarized {before_tokens} tokens]")),
            _ => {}
        }
    }
}

/// The input's most telling value: a command, path, pattern, query or prompt.
fn summary(input: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(input) else {
        return short(input, 80);
    };
    let keys = ["command", "file_path", "pattern", "query", "url", "description", "skill", "to", "prompt"];
    keys.iter().find_map(|k| map.get(*k).and_then(|v| v.as_str())).map_or_else(|| short(input, 80), |v| short(v, 80))
}

fn short(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Opts {
        let mut argv = vec!["eos-agent", "chat"];
        argv.extend_from_slice(args);
        let mut opts = Cli::try_parse_from(argv).expect("args").opts;
        // Not from the environment the tests run in.
        if !args.contains(&"--claude-store") {
            opts.claude_store = None;
        }
        opts
    }

    #[test]
    fn sandbox_flags_layer_over_the_config() {
        assert!(sandbox_config(&opts(&[])).expect("ok").is_none());
        let c = sandbox_config(&opts(&["--sandbox", "--allow-domain", "github.com"])).expect("ok").expect("some");
        assert!(c.enabled());
        assert_eq!(c.allowed_domains(), ["github.com"]);
        let c = sandbox_config(&opts(&[
            "--sandbox-config",
            r#"{"sandbox":{"enabled":true,"network":{"allowedDomains":["a.com"]}}}"#,
            "--allow-domain",
            "b.com",
        ]))
        .expect("ok")
        .expect("some");
        assert!(c.enabled());
        assert_eq!(c.allowed_domains(), ["a.com", "b.com"]);
        assert!(sandbox_config(&opts(&["--sandbox-config", "/no/such/file.json"])).is_err());
    }

    #[test]
    fn chat_is_interactive_and_run_is_not() {
        let run = agent_options(&opts(&[]), PathBuf::from("/"), false);
        assert_eq!(run.disallowed_tools, [ASK_USER_QUESTION]);
        assert!(!run.interactive, "a typed fork skill runs in the foreground, as in -p");
        let chat = agent_options(&opts(&[]), PathBuf::from("/"), true);
        assert!(chat.disallowed_tools.is_empty());
        assert!(chat.interactive, "a typed fork skill launches in the background");
    }

    #[test]
    fn usage_lines_add_up() {
        let usage = Usage {
            input_tokens: Some(1200),
            output_tokens: Some(34),
            cache_creation_input_tokens: Some(1_000_000),
            cache_read_input_tokens: Some(5),
            ..Usage::default()
        };
        let mut tally = Tally::default();
        tally.turn.add(&usage);
        assert_eq!(tally.end_turn().to_string(), "1,200 in, 34 out, 1,000,000 cache write, 5 cache read");
        tally.turn.add(&usage);
        tally.turn_subagents.add(&usage);
        assert!(
            tally
                .end_turn()
                .to_string()
                .ends_with("; subagents: 1,200 in, 34 out, 1,000,000 cache write, 5 cache read")
        );
        assert_eq!(tally.total().own.input, 2400);
        assert_eq!(tally.total().subagents.output, 34);
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
    }

    #[test]
    fn the_model_and_the_artifacts_get_their_own_credentials() {
        let p = PathBuf::from;
        let cases: [(&[&str], ModelAuth, Option<PathBuf>); 6] = [
            (&[], ModelAuth::Env, None),
            (&["--claude-store", "/s"], ModelAuth::Store(p("/s")), None),
            (&["--api-key-env", "K"], ModelAuth::ApiKeyEnv("K".into()), None),
            (&["--api-key-env", "K", "--claude-store", "/s"], ModelAuth::ApiKeyEnv("K".into()), Some(p("/s"))),
            (
                &["--api-key-env", "K", "--claude-store", "/s", "--artifacts-claude-store", "/a"],
                ModelAuth::ApiKeyEnv("K".into()),
                Some(p("/a")),
            ),
            (&["--claude-store", "/s", "--artifacts-claude-store", "/a"], ModelAuth::Store(p("/s")), Some(p("/a"))),
        ];
        for (args, model, artifacts) in cases {
            assert_eq!(credentials(&opts(args)), (model, artifacts), "{args:?}");
        }
    }

    #[tokio::test]
    async fn logout_forgets_the_named_chatgpt_store() {
        let store = std::env::temp_dir().join(format!("eos-cli-logout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).expect("dir");
        std::fs::write(store.join("siwc.json"), "{}").expect("record");
        let path = store.to_str().expect("utf-8");
        let cli = Cli::try_parse_from(["eos-agent", "logout", "chatgpt", "--store", path]).expect("args");
        let Command::Logout { provider, store: named } = cli.command else { panic!("logout") };
        assert_eq!(sign_in_store(provider, named.as_ref(), &cli.opts), store);
        assert_eq!(sign_in_store(provider, None, &opts(&["--chatgpt-store", "/c"])), PathBuf::from("/c"));

        logout(provider, &store).await.expect("signed out");
        assert!(!ChatGptLogin::exists(&store));
        logout(provider, &store).await.expect("nothing left to do");
        let _ = std::fs::remove_dir_all(&store);
    }
}
