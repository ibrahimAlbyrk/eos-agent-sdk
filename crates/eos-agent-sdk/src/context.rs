//! The context an agent's first message carries, in Claude Code's order:
//! environment, model, deferred tools, MCP notes, agent types, skills, date.

use std::path::PathBuf;

use eos_agent_core::{
    FailedServer, ServerInstructions, ServerNotes, SessionContext, Toolbox, instructions_section, server_paragraphs,
};
use eos_agent_mcp::{McpToolset, QUOTE_LIMIT, quoted, tool_name};
use eos_agent_prompt::{ModelInfo, Prompts, SessionContextBuilder};
use eos_agent_skills::{ListingBudget, SKILL_TOOL};

use crate::project::Project;
use crate::services::Services;
use crate::toolbox;

const TOOL_SEARCH: &str = "ToolSearch";

/// What differs between agents of one project.
pub(crate) struct ContextFacts<'a> {
    pub(crate) project: &'a Project,
    pub(crate) toolbox: &'a Toolbox,
    pub(crate) model: &'a str,
    pub(crate) email: Option<&'a str>,
    /// Top-level agents only: Claude Code's git snapshot.
    pub(crate) git_status: Option<&'a str>,
    pub(crate) include_claude_md: bool,
    /// The MCP servers as this agent's tools see them: their instructions,
    /// those still connecting, needing a sign-in or failed.
    pub(crate) servers: ServerNotes,
    pub(crate) listing_budget: ListingBudget,
    /// The agent's further working directories.
    pub(crate) roots: &'a [PathBuf],
    /// The tree's sandbox, when it is on.
    pub(crate) sandbox: Option<&'a std::sync::Arc<eos_agent_tools_shell::SandboxPolicy>>,
}

/// The agent's session context, shared with every live agent that has the same.
pub(crate) fn session_context(facts: &ContextFacts<'_>, services: &Services) -> SessionContext {
    // Claude Code's sandbox section leads the system message of agents with Bash.
    let sandbox = facts
        .sandbox
        .filter(|_| toolbox::has(facts.toolbox, "Bash"))
        .map(|s| s.prompt_section(&facts.project.cwd, facts.roots));
    let model = services.model_info(facts.model);
    let system_turns = services.system_turns(facts.model);
    let mut context = build(facts, &services.prompts, sandbox, model, system_turns);
    context.servers = facts.servers.clone();
    services.shared.share(context)
}

fn build(
    facts: &ContextFacts<'_>,
    prompts: &Prompts,
    sandbox: Option<String>,
    model: ModelInfo,
    system_turns: bool,
) -> SessionContext {
    let project = facts.project;
    let mut sections: Vec<String> = instructions_section(&facts.servers.instructions).into_iter().collect();
    if toolbox::has(facts.toolbox, "Agent") {
        sections.push(project.definitions.listing());
    }
    if toolbox::has(facts.toolbox, SKILL_TOOL)
        && let Some(listing) = project.skills.listing(&facts.listing_budget)
    {
        sections.push(listing);
    }
    let instructions = if facts.include_claude_md { project.instructions.clone() } else { Vec::new() };
    SessionContextBuilder::new(&project.cwd)
        .instructions(instructions)
        .user_email(facts.email.map(str::to_owned))
        .git_status(facts.git_status.map(str::to_owned))
        .model(model)
        .leading_sections(sandbox.into_iter().collect())
        .deferred_tools(toolbox::deferred_names(facts.toolbox))
        .deferred_notes(server_paragraphs(&facts.servers, !toolbox::has(facts.toolbox, TOOL_SEARCH)))
        .extra_sections(sections)
        .system_turns(system_turns)
        .build(prompts)
}

/// What the notes say about `mcp`'s servers to an agent with `toolbox`: their
/// instructions, those still connecting, and, as in Claude Code's sessions
/// that cannot run the OAuth flow, the servers waiting for a sign-in (but not
/// one whose auth tools the agent has, which stand in for it), and those that
/// failed to connect.
pub(crate) fn server_notes(mcp: Option<&McpToolset>, toolbox: &Toolbox) -> ServerNotes {
    let Some(mcp) = mcp else { return ServerNotes::default() };
    let stands_in = |server: &str| {
        ["authenticate", "complete_authentication"].iter().any(|t| toolbox.get(&tool_name(server, t)).is_some())
    };
    let needs_auth = mcp.needs_auth().into_iter().filter(|s| !stands_in(s)).collect();
    let failed = mcp
        .failures()
        .iter()
        .map(|f| FailedServer {
            name: quoted(&f.server, QUOTE_LIMIT),
            error_code: None,
            error: Some(quoted(&f.error, QUOTE_LIMIT)),
        })
        .collect();
    let instructions = mcp
        .instructions()
        .into_iter()
        .map(|i| ServerInstructions { server: i.server, text: i.text.to_string() })
        .collect();
    ServerNotes { needs_auth, failed, pending: mcp.pending(), connected: mcp.servers(), instructions }.sorted()
}
