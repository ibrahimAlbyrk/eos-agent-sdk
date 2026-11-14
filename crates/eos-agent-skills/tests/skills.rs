use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eos_agent_core::{BoxFuture, CancellationToken, Extensions, ReminderSource, Reminders, Tool, ToolContext};
use eos_agent_skills::{
    CommandRunner, ExpandContext, ListingBudget, NoCommands, Origin, PendingGrants, ShellFailure, Skill, SkillCatalog,
    SkillListingReminder, SkillRenderer, SkillRoots, SkillTool,
};
use eos_agent_types::ToolResultContent;
use serde_json::value::RawValue;

/// A directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "eos-skills-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, text).expect("write");
        path
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn skill_md(description: &str, body: &str) -> String {
    format!("---\ndescription: {description}\n---\n{body}")
}

/// home/.claude (user), home/outer/.claude (above the git root, not read),
/// home/outer/proj (git root) with .claude, and proj/app/.claude nearer.
fn layout() -> (TempDir, SkillRoots) {
    let t = TempDir::new();
    t.write("home/.claude/skills/deploy/SKILL.md", &skill_md("User deploy", "user body"));
    t.write("home/.claude/commands/frontend/component.md", "Make a component for $ARGUMENTS");
    t.write("home/outer/.claude/skills/outside/SKILL.md", &skill_md("Above the repo", "x"));
    t.write("home/outer/proj/.git/HEAD", "ref: main");
    t.write("home/outer/proj/.claude/skills/deploy/SKILL.md", &skill_md("Project deploy", "project body"));
    t.write("home/outer/proj/.claude/skills/review/SKILL.md", &skill_md("Root review", "root"));
    t.write("home/outer/proj/app/.claude/skills/review/SKILL.md", &skill_md("App review", "app"));
    t.write("home/outer/proj/app/.claude/skills/review/.claude-plugin/plugin.json", "{}");
    t.write("home/outer/proj/app/.claude/skills/lint/SKILL.md", &skill_md("App lint", "lint $0"));
    std::fs::create_dir_all(t.path("home/outer/proj/app/src")).expect("dirs");
    let roots = SkillRoots {
        user_dir: Some(t.path("home/.claude")),
        cwd: Some(t.path("home/outer/proj/app/src")),
        home: Some(t.path("home")),
        plugin_dirs: Vec::new(),
        installed_plugins: true,
        bundled: true,
        artifacts: None,
        extra: Vec::new(),
    };
    (t, roots)
}

#[test]
fn discovery_precedence_and_walk() {
    let (_t, roots) = layout();
    let catalog = SkillCatalog::discover(&roots);
    // The user's skill shadows the project's one of the same name.
    let deploy = catalog.get("deploy").expect("deploy");
    assert_eq!(deploy.description, "User deploy");
    assert_eq!(deploy.origin, Origin::UserSkill);
    // A folder holding a plugin is not a skill; the repo root's one is found.
    assert_eq!(catalog.get("review").expect("review").description, "Root review");
    assert_eq!(catalog.get("lint").expect("lint").origin, Origin::ProjectSkill);
    // Nothing above the git root.
    assert!(catalog.get("outside").is_none());
    let component = catalog.get("frontend:component").expect("legacy command");
    assert_eq!(component.description, "Make a component for $ARGUMENTS");
    assert_eq!(component.origin, Origin::UserCommand);
    assert!(catalog.get("design").is_some_and(|s| s.origin == Origin::Bundled));
}

#[test]
fn plugins_are_named_and_enabled_by_settings() {
    let t = TempDir::new();
    let plugin = t.path("cache/tools");
    t.write("cache/tools/.claude-plugin/plugin.json", r#"{"name":"tools"}"#);
    t.write("cache/tools/skills/fmt/SKILL.md", &skill_md("Format code", "Run ${CLAUDE_PLUGIN_ROOT}/fmt.sh"));
    t.write("cache/tools/commands/release.md", "Cut a release");
    t.write("cache/other/.claude-plugin/plugin.json", r#"{"name":"other"}"#);
    t.write("cache/other/skills/x/SKILL.md", &skill_md("X", "x"));
    let installed = serde_json::json!({"version": 2, "plugins": {
        "tools@market": [{"scope": "user", "installPath": plugin}],
        "other@market": [{"scope": "user", "installPath": t.path("cache/other")}],
    }});
    t.write("user/plugins/installed_plugins.json", &installed.to_string());
    t.write("user/settings.json", r#"{"enabledPlugins":{"tools@market":true,"other@market":false}}"#);
    let roots = SkillRoots { user_dir: Some(t.path("user")), installed_plugins: true, ..SkillRoots::default() };
    let catalog = SkillCatalog::discover(&roots);

    let fmt = catalog.get("tools:fmt").expect("plugin skill");
    assert_eq!(fmt.origin, Origin::PluginSkill { plugin: "tools".into() });
    assert_eq!(fmt.plugin_root.as_deref(), Some(plugin.as_path()));
    assert!(catalog.get("tools:release").is_some());
    assert!(catalog.get("other:x").is_none());
    // A bare name reaches a plugin item when only one plugin has it.
    assert_eq!(catalog.get("fmt").expect("alias").name, "tools:fmt");
    // Plugin commands without their own description stay out of the listing.
    let listing = catalog.listing(&ListingBudget::default()).expect("listing");
    assert!(listing.contains("- tools:fmt: Format code"));
    assert!(!listing.contains("tools:release"));
}

#[tokio::test]
async fn expansion_fills_arguments_and_base_dir() {
    let t = TempDir::new();
    let dir = t.path("user/skills/ship");
    t.write(
        "user/skills/ship/SKILL.md",
        "---\ndescription: Ship\narguments: [env, region]\nallowed-tools: Bash(git push:*)\nmodel: claude-x\n---\n\
         Ship to $env in $region ($ARGUMENTS[0]/$1). All: $ARGUMENTS. Dir: ${CLAUDE_SKILL_DIR}",
    );
    let roots = SkillRoots { user_dir: Some(t.path("user")), ..SkillRoots::default() };
    let catalog = SkillCatalog::discover(&roots);
    let ctx = ExpandContext::new(&NoCommands);
    let exp = catalog.expand("/ship prod \"eu west\"", &ctx).await.expect("a skill").expect("expands");
    assert_eq!(
        exp.visible,
        "<command-message>ship</command-message>\n<command-name>/ship</command-name>\n<command-args>prod \"eu west\"</command-args>"
    );
    assert_eq!(
        exp.body,
        format!(
            "Base directory for this skill: {d}\n\nShip to prod in eu west (prod/eu west). All: prod \"eu west\". Dir: {d}",
            d = dir.display()
        )
    );
    assert_eq!(exp.allowed_tools, ["Bash(git push:*)"]);
    assert_eq!(exp.model.as_deref(), Some("claude-x"));
    assert!(catalog.expand("/unknown x", &ctx).await.is_none());
    assert!(catalog.expand("not a command", &ctx).await.is_none());
}

#[tokio::test]
async fn design_is_user_only() {
    let catalog = SkillCatalog::discover(&SkillRoots { bundled: true, ..SkillRoots::default() });
    let ctx = ExpandContext::new(&NoCommands);
    let exp = catalog.expand("/design a pricing page", &ctx).await.expect("design").expect("expands");
    assert!(exp.body.starts_with("`/design` was invoked: a request for a design made as a NEW Artifact"));
    assert!(exp.body.contains("Call the `Artifact` tool with `action: \"quickstart\"` and `intent: \"design\"`"));
    assert!(exp.body.ends_with("The brief:\n\na pricing page"));
    let slides = catalog.expand("/slides", &ctx).await.expect("slides").expect("expands");
    assert!(slides.body.ends_with("ask the user what the deck should be about before creating anything."));
    assert!(catalog.listing(&ListingBudget::default()).is_none(), "user-only commands are not listed");
    let refused = catalog.invoke("design", "", &ctx).await.expect_err("model may not invoke");
    assert!(
        refused.to_string().starts_with("Skill design cannot be used with Skill tool due to disable-model-invocation.")
    );
}

/// Stands in for the frame API's runtime contract.
#[derive(Debug)]
struct Contract;
impl SkillRenderer for Contract {
    fn render<'a>(&'a self, args: &'a str, _call: Option<eos_agent_core::ToolContext<'a>>) -> BoxFuture<'a, String> {
        Box::pin(async move { format!("# Artifact runtime capabilities [{args}]") })
    }
}

#[tokio::test]
async fn artifact_skills_come_with_the_artifact_tools() {
    let without = SkillCatalog::discover(&SkillRoots { bundled: true, ..SkillRoots::default() });
    assert!(without.get("artifact-design").is_none());

    let roots = SkillRoots { bundled: true, artifacts: Some(Arc::new(Contract)), ..SkillRoots::default() };
    let catalog = SkillCatalog::discover(&roots);
    // Listed as Claude Code lists them, in its order.
    let listing = catalog.listing(&ListingBudget::default()).expect("listing");
    let lines: Vec<&str> = listing.lines().skip(2).collect();
    assert_eq!(lines.len(), 3, "{listing}");
    assert_eq!(
        lines[0],
        "- artifact-design: Design guidance and fundamentals for Artifacts. - Load before writing any artifact, \
including a skill-instructed Markdown one - Markdown is never a shortcut past the design pass."
    );
    assert_eq!(
        lines[1],
        "- artifact-diagramming: Diagramming know-how for Artifacts - when a picture earns its place, how to draw one \
that shows the real mechanism, and the inline-SVG mechanics that keep it legible in both themes."
    );
    assert_eq!(
        lines[2],
        "- artifact-capabilities: Runtime capabilities a published Artifact page can be granted \u{2014} behavior \
static HTML cannot provide on its own, such as the page reading live or connected data, remembering what people do on \
it (a poll, a sign-up sheet, a checklist, a document edited in place \u{2014} it saves new versions of itself), keeping \
state shared across viewers, knowing who is viewing, asking Claude a question of its own, storing files people add, \
handing the viewer a file to save, or using the viewer's camera, microphone, location, screen or device motion. Serves \
this user's live capability roster and the typed call definitions. Load it whenever any such runtime behavior would \
make an artifact more useful, before writing the page."
    );

    // The model loads them; only the last two are also user commands.
    let ctx = ExpandContext::new(&NoCommands);
    let design = catalog.invoke("artifact-design", "", &ctx).await.expect("loads");
    assert!(design.body.starts_with("## Page contract \u{2014} read before your first publish"));
    assert!(design.body.contains("## Read the request first"));
    assert!(catalog.expand("/artifact-design", &ctx).await.is_none(), "not a user command");
    let diagramming = catalog.expand("/artifact-diagramming", &ctx).await.expect("command").expect("expands");
    assert!(diagramming.body.starts_with("Draw as the engineer who has to live with the decision"));
    let capabilities = catalog.invoke("artifact-capabilities", "x", &ctx).await.expect("loads");
    assert_eq!(capabilities.body, "# Artifact runtime capabilities [x]");
}

#[derive(Debug)]
struct Shell;
impl CommandRunner for Shell {
    fn run<'a>(&'a self, command: &'a str, _: Option<ToolContext<'a>>) -> BoxFuture<'a, Result<String, ShellFailure>> {
        Box::pin(async move { Ok(format!("[ran {command}]")) })
    }
}

#[tokio::test]
async fn skill_tool_loads_through_the_same_path() {
    let catalog = Arc::new(SkillCatalog::from_skills(vec![Skill::from_markdown(
        "status",
        "---\ndescription: Repo status\nallowed-tools: Bash(git status:*)\neffort: low\n---\nState: !`git status`\nFocus on $ARGUMENTS",
        Origin::ProjectSkill,
        None,
    )]));
    let tool = SkillTool::new(Arc::clone(&catalog), Arc::new(Shell));
    let extensions = Extensions::default();
    let cancel = CancellationToken::new();
    let call = |input: &str| {
        let raw = RawValue::from_string(input.to_owned()).expect("json");
        (
            raw,
            ToolContext {
                agent_id: "a1",
                tool_use_id: "t1",
                cancel: &cancel,
                extensions: &extensions,
                transcript: &[],
            },
        )
    };
    let (raw, ctx) = call(r#"{"skill":"/status","args":"tests"}"#);
    let out = tool.call(&raw, ctx).await;
    assert!(!out.is_error);
    assert!(matches!(&out.content, ToolResultContent::Text(t) if t == "Launching skill: status"));
    assert_eq!(extensions.get_or_default::<Reminders>().take(), ["State: [ran git status]\nFocus on tests"]);
    let grants = extensions.get_or_default::<PendingGrants>().take();
    assert_eq!(grants[0].allowed_tools, ["Bash(git status:*)"]);
    assert_eq!(grants[0].effort.as_deref(), Some("low"));

    let (raw, ctx) = call(r#"{"skill":"statsu"}"#);
    let out = tool.call(&raw, ctx).await;
    assert!(out.is_error);
    assert!(matches!(&out.content, ToolResultContent::Text(t)
        if t == "<tool_use_error>Unknown skill: statsu. Did you mean status?</tool_use_error>"));
}

#[test]
fn listing_budget_and_deltas() {
    let many: Vec<Skill> = (0..60)
        .map(|i| {
            Skill::from_markdown(&format!("skill-{i:02}"), &skill_md(&"d".repeat(200), ""), Origin::UserSkill, None)
        })
        .collect();
    let catalog = SkillCatalog::from_skills(many);
    let listing = catalog.listing(&ListingBudget::default()).expect("listing");
    let entries = listing.split_once("\n\n").expect("header").1;
    assert!(entries.chars().count() <= 8000);
    assert_eq!(entries.lines().count(), 60);
    assert!(entries.lines().any(|l| l == "- skill-59"), "later skills collapse to their names");

    let catalog = Arc::new(SkillCatalog::from_skills(vec![Skill::from_markdown(
        "a",
        &skill_md("first", ""),
        Origin::UserSkill,
        None,
    )]));
    let reminder = SkillListingReminder::new(Arc::clone(&catalog), ListingBudget::default(), true);
    let ext = Extensions::default();
    let collect = |ext: &Extensions| futures_executor(reminder.collect(ext));
    assert!(collect(&ext).is_empty(), "the initial listing went out with the session context");
    catalog.replace(vec![
        Skill::from_markdown("a", &skill_md("first", ""), Origin::UserSkill, None),
        Skill::from_markdown("b", &skill_md("second", ""), Origin::UserSkill, None),
    ]);
    assert_eq!(
        collect(&ext),
        [
            "<system-reminder>\nThe following skills are available for use with the Skill tool:\n\n- b: second\n</system-reminder>"
        ]
    );
    assert!(collect(&ext).is_empty());
}

fn futures_executor<T>(fut: BoxFuture<'_, T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().expect("runtime").block_on(fut)
}

#[test]
fn files_are_capped_and_symlinks_load_once() {
    let t = TempDir::new();
    t.write("user/skills/real/SKILL.md", &skill_md("Real", "x"));
    t.write("user/skills/huge/SKILL.md", &"x".repeat(1_000_001));
    #[cfg(unix)]
    std::os::unix::fs::symlink(t.path("user/skills/real"), t.path("user/skills/zlink")).expect("symlink");
    let catalog = SkillCatalog::discover(&SkillRoots { user_dir: Some(t.path("user")), ..SkillRoots::default() });
    assert!(catalog.get("real").is_some());
    assert!(catalog.get("huge").is_none());
    assert!(catalog.get("zlink").is_none());
    assert_eq!(catalog.skills().len(), 1);
}
