//! Skills of a project's subdirectories, found as the agent touches files
//! there, and skills that apply to certain paths.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eos_agent_core::{BoxFuture, Extensions, FileStamp, FileStates, ReminderSource};
use eos_agent_skills::{ExpandContext, ListingBudget, NoCommands, SkillCatalog, SkillListingReminder, SkillRoots};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "eos-scoped-skills-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(std::fs::canonicalize(&dir).expect("canonical"))
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, text).expect("write");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn skill(description: &str, body: &str) -> String {
    format!("---\ndescription: {description}\n---\n{body}")
}

fn run<T>(fut: BoxFuture<'_, T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().expect("runtime").block_on(fut)
}

fn touch(ext: &Extensions, path: &Path) {
    let stamp = FileStamp { modified_ms: 0, len: 0, hash: 0, partial: false };
    ext.get_or_default::<FileStates>().record(path.to_path_buf(), stamp);
}

/// `proj` (a git repository ignoring `vendor/`) with an unscoped `deploy`, a
/// `py-style` skill for Python files, and skills in `apps/web`, `apps/api`
/// and the ignored `vendor/lib`.
fn project() -> (TempDir, Arc<SkillCatalog>) {
    let t = TempDir::new();
    let proj = t.0.join("proj");
    std::fs::create_dir_all(&proj).expect("proj");
    let git = Command::new("git").args(["init", "-q"]).current_dir(&proj).status().expect("git");
    assert!(git.success());
    t.write("proj/.gitignore", "vendor/\n");
    t.write("proj/.claude/skills/deploy/SKILL.md", &skill("Deploy everything", "root deploy"));
    t.write("proj/.claude/skills/py-style/SKILL.md", "---\ndescription: Python style\npaths: \"**/*.py\"\n---\npy");
    t.write("proj/apps/web/.claude/skills/deploy/SKILL.md", &skill("Deploy the web app", "web deploy"));
    t.write("proj/apps/web/.claude/skills/lint/SKILL.md", &skill("Lint the web app", "web lint"));
    t.write("proj/apps/api/.claude/skills/lint/SKILL.md", &skill("Lint the API", "api lint"));
    t.write("proj/vendor/lib/.claude/skills/secret/SKILL.md", &skill("Vendored", "x"));
    let roots = SkillRoots { cwd: Some(proj), home: Some(t.0.clone()), ..SkillRoots::default() };
    (t, Arc::new(SkillCatalog::discover(&roots)))
}

fn names(catalog: &SkillCatalog) -> Vec<String> {
    let mut n: Vec<String> = catalog.listed().iter().map(|s| s.name.clone()).collect();
    n.sort();
    n
}

#[test]
fn scoped_skills_appear_once_a_file_under_their_folder_is_touched() {
    let (t, catalog) = project();
    assert_eq!(names(&catalog), ["deploy"], "nothing below the root is listed before a file there is touched");

    let reminder = SkillListingReminder::new(Arc::clone(&catalog), ListingBudget::default(), true);
    let ext = Extensions::default();
    assert!(run(reminder.collect(&ext)).is_empty());
    touch(&ext, &t.0.join("proj/apps/web/src/main.ts"));
    let notes = run(reminder.collect(&ext));
    assert_eq!(
        notes[0],
        "<system-reminder>\nNew skills discovered in apps/web/.claude/skills, now available via the Skill tool:\n- apps/web:deploy\n- lint\n</system-reminder>"
    );
    assert!(notes[1].contains(
        "- apps/web:deploy: Deploy the web app (scoped to apps/web/ \u{2014} use this instead of the unscoped \"deploy\" skill when the files being changed are under apps/web/)"
    ), "{}", notes[1]);
    assert!(notes[1].contains("- lint: Lint the web app (from apps/web/.claude/skills \u{2014} applies when working on files under apps/web/)"), "{}", notes[1]);
    assert!(run(reminder.collect(&ext)).is_empty(), "each folder is found once");

    // A second `lint` elsewhere: both are listed by their folder.
    touch(&ext, &t.0.join("proj/apps/api/handler.ts"));
    let notes = run(reminder.collect(&ext));
    assert!(
        notes[0].contains("New skills discovered in apps/api/.claude/skills") && notes[0].contains("- apps/api:lint"),
        "{notes:?}"
    );
    assert_eq!(names(&catalog), ["apps/api:lint", "apps/web:deploy", "apps/web:lint", "deploy"]);

    // Git-ignored folders are skipped; files outside the project add nothing.
    touch(&ext, &t.0.join("proj/vendor/lib/a.rs"));
    touch(&ext, &t.0.join("elsewhere/.claude/skills/x/SKILL.md"));
    assert!(run(reminder.collect(&ext)).is_empty());
    assert!(catalog.get("secret").is_none() && catalog.get("vendor/lib:secret").is_none());
}

#[test]
fn the_scoped_name_loads_its_own_skill() {
    let (t, catalog) = project();
    catalog.touched(&[t.0.join("proj/apps/web/index.ts")]);
    let runner = NoCommands;
    let ctx = ExpandContext::new(&runner);
    let web = run(Box::pin(catalog.invoke("apps/web:deploy", "", &ctx))).expect("scoped");
    assert!(web.body.contains("web deploy"), "{}", web.body);
    let root = run(Box::pin(catalog.invoke("deploy", "", &ctx))).expect("unscoped");
    assert!(root.body.contains("root deploy"), "{}", root.body);
}

#[test]
fn a_path_skill_is_listed_once_a_matching_file_is_touched() {
    let (t, catalog) = project();
    assert!(!names(&catalog).contains(&"py-style".to_owned()));
    catalog.touched(&[t.0.join("proj/README.md")]);
    assert!(!names(&catalog).contains(&"py-style".to_owned()));
    catalog.touched(&[t.0.join("proj/tools/build.py")]);
    assert!(names(&catalog).contains(&"py-style".to_owned()));
}

/// Finding skills never walks the tree: only the folders above the files
/// touched are looked at. Run with `--ignored` to see the cost on a tree of
/// 50,000 files.
#[test]
#[ignore = "measures; prints timings"]
fn cost_on_a_large_tree() {
    let t = TempDir::new();
    let proj = t.0.join("proj");
    for i in 0..500 {
        for j in 0..100 {
            t.write(&format!("proj/pkg{i}/src/mod{}/f{j}.ts", j % 10), "x");
        }
    }
    t.write("proj/pkg7/.claude/skills/build/SKILL.md", &skill("Build pkg7", "b"));
    let roots = SkillRoots { cwd: Some(proj.clone()), ..SkillRoots::default() };
    let start = std::time::Instant::now();
    let catalog = SkillCatalog::discover(&roots);
    let discover = start.elapsed();
    let files: Vec<PathBuf> = (0..256).map(|i| proj.join(format!("pkg{}/src/mod{}/f{i}.ts", i % 50, i % 10))).collect();
    let start = std::time::Instant::now();
    let found = catalog.touched(&files);
    let first = start.elapsed();
    let start = std::time::Instant::now();
    catalog.touched(&files);
    let again = start.elapsed();
    println!("50k files: discover {discover:?}; 256 touched files {first:?} (found {found:?}); again {again:?}");
}
