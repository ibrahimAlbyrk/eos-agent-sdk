//! Skills and slash commands, as Claude Code has them.
//!
//! - Discovery ([`SkillCatalog::discover`] over [`SkillRoots`]): user and
//!   project `.claude/skills/<name>/SKILL.md`, legacy `.claude/commands/*.md`,
//!   plugin `skills/` and `commands/` (named `plugin:item`), the bundled
//!   user-only `/design` and `/slides`, and with the Artifact tools the
//!   bundled `artifact-design`, `artifact-diagramming` and
//!   `artifact-capabilities`.
//! - The listing the model reads ([`SkillCatalog::listing`]) and a
//!   [`SkillListingReminder`] that announces skills added later.
//! - One expansion path for `/name args` typed by the user
//!   ([`SkillCatalog::expand`]) and the [`SkillTool`]: base directory line,
//!   argument placeholders, inline `` !`cmd` `` through a [`CommandRunner`],
//!   plus the frontmatter's tool grants, model and effort as data.

mod args;
mod bundled;
mod catalog;
mod discover;
mod expand;
mod fork;
mod frontmatter;
mod inline;
mod listing;
mod plugins;
mod skill;
mod tool;

pub use bundled::SkillRenderer;
pub use catalog::{InvokeError, SkillCatalog, parse_slash};
pub use discover::{BundledSkill, SkillRoots};
pub use expand::{ExpandContext, ExpandError, Expansion};
pub use fork::{
    agent_name, background_fork_request, fork_request, forked_command_id, forked_command_input,
    forked_command_launched, typed_line,
};
pub use inline::{CommandRunner, NoCommands, ShellFailure};
pub use listing::{LISTING_HEADER, ListingBudget, SkillListingReminder};
pub use skill::{Origin, Skill};
pub use tool::{PendingGrants, SKILL_TOOL, SkillGrant, SkillTool};
