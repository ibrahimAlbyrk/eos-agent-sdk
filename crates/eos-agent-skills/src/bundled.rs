//! Claude Code's bundled skills that this engine ships:
//!
//! - the user-only commands `/design` and `/slides`, which route into the
//!   Artifact tool's quickstart → type → fill flow (the design guidance then
//!   arrives from the server);
//! - with the Artifact tools, the skills their descriptions tell the model to
//!   load: `artifact-design` (its SKILL file from Claude Code 2.1.319, after
//!   the Artifact tool's page contract), `artifact-diagramming` (its SKILL
//!   file) and `artifact-capabilities` (composed per load from the frame
//!   API's runtime contract by a [`SkillRenderer`] the host supplies).

use std::sync::Arc;

use eos_agent_core::{BoxFuture, ToolContext};

use crate::frontmatter;
use crate::skill::{Body, Origin, Skill};

const DESIGN: &str = include_str!("../data/design.md");
const SLIDES: &str = include_str!("../data/slides.md");
const ARTIFACT_DESIGN: &str = include_str!("../data/artifact-design/SKILL.md");
/// The Artifact tool's own rules for the published file, which Claude Code
/// puts ahead of the design guidance when the artifact tools are separate
/// tools (as here).
const PAGE_CONTRACT: &str = include_str!("../data/artifact-design/page-contract.md");
const ARTIFACT_DIAGRAMMING: &str = include_str!("../data/artifact-diagramming/SKILL.md");
/// Filled with the `dataviz` skill's pointer only when that skill exists.
const DATAVIZ_CALLOUT: &str = "<!-- dataviz-callout -->";
/// The design guidance's reference to the tool description, which the page
/// contract above it now holds.
const CDN_HOSTS: &str = "the Artifact tool's description lists the few other script hosts the CSP admits";
const CDN_HOSTS_HERE: &str = "the page contract above lists the few other script hosts the CSP admits";
const CAPABILITIES_DESCRIPTION: &str = "Runtime capabilities a published Artifact page can be granted \u{2014} behavior \
static HTML cannot provide on its own, such as the page reading live or connected data, remembering what people do on \
it (a poll, a sign-up sheet, a checklist, a document edited in place \u{2014} it saves new versions of itself), keeping \
state shared across viewers, knowing who is viewing, asking Claude a question of its own, storing files people add, \
handing the viewer a file to save, or using the viewer's camera, microphone, location, screen or device motion. Serves \
this user's live capability roster and the typed call definitions. Load it whenever any such runtime behavior would make \
an artifact more useful, before writing the page.";

/// Composes a bundled skill's text when it is loaded (the frame API's
/// runtime contract for `artifact-capabilities`).
pub trait SkillRenderer: Send + Sync + std::fmt::Debug {
    /// `call` is the agent the skill loads for, when there is one.
    fn render<'a>(&'a self, args: &'a str, call: Option<ToolContext<'a>>) -> BoxFuture<'a, String>;
}

/// Words after `/design` that name a Claude Design account or project
/// command rather than a brief.
const DESIGN_SUBCOMMANDS: [&str; 7] = ["sync", "login", "consent", "revoke", "import", "export", "status"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bundled {
    Design,
    Slides,
    ArtifactDesign,
    ArtifactDiagramming,
}

pub(crate) fn all() -> Vec<Skill> {
    vec![
        Skill::bundled("design", "Make a new Design artifact from a brief", "[what to design]", Bundled::Design),
        Skill::bundled(
            "slides",
            "Make a new Slides deck artifact from a brief",
            "[what the deck is about]",
            Bundled::Slides,
        ),
    ]
}

/// The Artifact skills, in Claude Code's order; `capabilities` composes
/// `artifact-capabilities`.
pub(crate) fn artifacts(capabilities: Arc<dyn SkillRenderer>) -> Vec<Skill> {
    let from_file = |name: &str, text: &str, kind: Bundled, user_invocable: bool| {
        let mut skill = Skill::from_markdown(name, text, Origin::Bundled, None);
        skill.user_invocable = user_invocable;
        skill.body = Body::Bundled(kind);
        skill.digest = skill.compute_digest();
        skill
    };
    let mut capabilities_skill = Skill::from_markdown("artifact-capabilities", "", Origin::Bundled, None);
    capabilities_skill.description = CAPABILITIES_DESCRIPTION.to_owned();
    capabilities_skill.has_user_description = true;
    capabilities_skill.body = Body::Rendered(capabilities);
    capabilities_skill.digest = capabilities_skill.compute_digest();
    vec![
        from_file("artifact-design", ARTIFACT_DESIGN, Bundled::ArtifactDesign, false),
        from_file("artifact-diagramming", ARTIFACT_DIAGRAMMING, Bundled::ArtifactDiagramming, true),
        capabilities_skill,
    ]
}

/// A host's bundled skill, composed by its renderer when loaded.
pub(crate) fn rendered(extra: &crate::discover::BundledSkill) -> Skill {
    let mut skill = Skill::from_markdown(&extra.name, "", Origin::Bundled, None);
    skill.description.clone_from(&extra.description);
    skill.has_user_description = true;
    skill.user_invocable = extra.user_invocable;
    skill.body = Body::Rendered(Arc::clone(&extra.renderer));
    skill.digest = skill.compute_digest();
    skill
}

pub(crate) fn render(kind: Bundled, args: &str) -> String {
    match kind {
        Bundled::Design => design(args),
        Bundled::Slides => brief(SLIDES, "what the deck should be about", args),
        Bundled::ArtifactDesign => {
            let body = instructions(ARTIFACT_DESIGN).replacen(DATAVIZ_CALLOUT, "", 1);
            format!("{PAGE_CONTRACT}{}", body.replacen(CDN_HOSTS, CDN_HOSTS_HERE, 1))
        }
        Bundled::ArtifactDiagramming => instructions(ARTIFACT_DIAGRAMMING).to_owned(),
    }
}

/// A SKILL file's body: no frontmatter, no leading blank lines.
fn instructions(text: &str) -> &str {
    frontmatter::split(text).1.trim_start()
}

fn brief(head: &str, ask: &str, args: &str) -> String {
    let brief = args.trim();
    if brief.is_empty() {
        format!("{head}\n\nNo brief was given \u{2014} ask the user {ask} before creating anything.")
    } else {
        format!("{head}\n\nThe brief:\n\n{brief}")
    }
}

/// `/design import|export|status|sync|login|consent|revoke` are Claude Design
/// account commands this session does not offer; anything else is a brief.
fn design(args: &str) -> String {
    let all = args.trim();
    let first = all.split_whitespace().next().unwrap_or("");
    let word = first.to_lowercase();
    let rest = all[first.len()..].trim();
    if DESIGN_SUBCOMMANDS.contains(&word.as_str()) {
        let not_offered = || {
            invoked(&format!(
                "\"{all}\" is a Claude Design account or project command, not a brief, and this session does not offer \
it (for import, export or status, claude.ai/design is the place). Tell the user that in one line and stop \u{2014} do \
not make anything named \"{all}\"."
            ))
        };
        if word == "sync" || (word == "import" && !rest.is_empty()) {
            return not_offered();
        }
        if rest.is_empty() && matches!(word.as_str(), "login" | "consent" | "revoke") {
            return invoked(&format!(
                "`/design {word}` is for the user to type themselves, in an interactive Claude Code terminal signed in \
to claude.ai; if they already did, this session does not offer it (organization policy or sign-in). Say so in one \
line and stop."
            ));
        }
        if rest.is_empty() {
            return not_offered();
        }
    }
    brief(DESIGN, "what they want designed", args)
}

fn invoked(text: &str) -> String {
    format!("`/design` was invoked. {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_routes_account_commands() {
        assert!(design("status").contains("\"status\" is a Claude Design account or project command"));
        assert!(design("sync Acme").contains("\"sync Acme\" is a Claude Design"));
        assert!(design("login").contains("`/design login` is for the user to type themselves"));
        assert!(design("export a landing page").ends_with("The brief:\n\nexport a landing page"));
        assert!(design("").ends_with("ask the user what they want designed before creating anything."));
        assert!(render(Bundled::Slides, " Q3 review ").ends_with("The brief:\n\nQ3 review"));
    }

    #[test]
    fn artifact_design_follows_the_page_contract() {
        let text = render(Bundled::ArtifactDesign, "");
        assert!(text.starts_with("## Page contract \u{2014} read before your first publish\n\n"));
        assert!(text.contains("\n\nWork the way the design lead at a small, versatile studio would"));
        assert!(!text.contains("---\nname: artifact-design"), "no frontmatter");
        assert!(!text.contains(DATAVIZ_CALLOUT) && !text.contains(CDN_HOSTS) && text.contains(CDN_HOSTS_HERE));
        assert!(render(Bundled::ArtifactDiagramming, "").starts_with("Draw as the engineer"));
    }
}
