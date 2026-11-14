use std::path::{Path, PathBuf};

use eos_agent_compat::{ClaudePaths, MemoryScope, discover_claude_md, read_global_config};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("eos-compat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, text).expect("write");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn paths(root: &Path) -> ClaudePaths {
    ClaudePaths { managed_dir: root.join("managed"), ..ClaudePaths::for_home(&root.join("home")) }
}

/// Paths relative to the test root, so ancestors outside it never matter.
fn found(root: &Path, cwd: &Path, allow_external: bool) -> Vec<(String, MemoryScope, String)> {
    discover_claude_md(&paths(root), cwd, allow_external)
        .into_iter()
        .filter(|f| f.path.starts_with(root))
        .map(|f| {
            let rel = f.path.strip_prefix(root).expect("under root").display().to_string();
            (rel, f.scope, f.content.trim().to_owned())
        })
        .collect()
}

#[test]
fn discovers_in_claude_code_order() {
    let t = TempDir::new("order");
    t.write("managed/CLAUDE.md", "policy");
    t.write("home/.claude/CLAUDE.md", "user @~/notes.md");
    t.write("home/notes.md", "notes");
    t.write("home/.claude/rules/style.md", "user rule");
    t.write("repo/CLAUDE.md", "---\ntitle: x\n---\nproject <!-- inline --> @docs/a.md\n<!-- hidden -->\n");
    t.write("repo/docs/a.md", "a imports @./b.md and @a.md");
    t.write("repo/docs/b.md", "b");
    t.write("repo/.claude/CLAUDE.md", "dot claude");
    t.write("repo/.claude/rules/b-rule.md", "rule b");
    t.write("repo/.claude/rules/a-rule.md", "rule a");
    t.write("repo/.claude/rules/scoped.md", "---\npaths:\n  - src/**\n---\nscoped");
    t.write("repo/CLAUDE.local.md", "local");
    t.write("repo/sub/CLAUDE.md", "sub");
    t.write("repo/sub/empty/CLAUDE.md", "  \n<!-- only a comment -->\n");
    t.write("elsewhere/CLAUDE.md", "not an ancestor");

    // Project files may import only from inside the working directory
    // unless the user approved external imports, as in Claude Code.
    let cwd = t.0.join("repo/sub/empty");
    let got = found(&t.0, &cwd, true);
    let expect = [
        ("managed/CLAUDE.md", MemoryScope::Managed, "policy"),
        ("home/.claude/CLAUDE.md", MemoryScope::User, "user @~/notes.md"),
        ("home/notes.md", MemoryScope::User, "notes"),
        ("home/.claude/rules/style.md", MemoryScope::User, "user rule"),
        ("repo/CLAUDE.md", MemoryScope::Project, "project <!-- inline --> @docs/a.md"),
        ("repo/docs/a.md", MemoryScope::Project, "a imports @./b.md and @a.md"),
        ("repo/docs/b.md", MemoryScope::Project, "b"),
        ("repo/.claude/CLAUDE.md", MemoryScope::Project, "dot claude"),
        ("repo/.claude/rules/a-rule.md", MemoryScope::Project, "rule a"),
        ("repo/.claude/rules/b-rule.md", MemoryScope::Project, "rule b"),
        ("repo/CLAUDE.local.md", MemoryScope::Local, "local"),
        ("repo/sub/CLAUDE.md", MemoryScope::Project, "sub"),
    ];
    let expect: Vec<(String, MemoryScope, String)> =
        expect.iter().map(|(p, s, c)| ((*p).to_owned(), *s, (*c).to_owned())).collect();
    assert_eq!(got, expect);

    let without: Vec<String> = found(&t.0, &cwd, false).into_iter().map(|(p, ..)| p).collect();
    let expect_without: Vec<String> =
        expect.iter().map(|(p, ..)| p.clone()).filter(|p| !p.starts_with("repo/docs/")).collect();
    assert_eq!(without, expect_without);
}

#[test]
fn imports_are_depth_limited_and_stay_in_the_project() {
    let t = TempDir::new("imports");
    t.write("repo/CLAUDE.md", "root @one.md @../outside.md @image.png");
    for (i, next) in ["one", "two", "three", "four", "five", "six"].windows(2).enumerate() {
        t.write(&format!("repo/{}.md", next[0]), &format!("level {} @{}.md", i + 1, next[1]));
    }
    t.write("repo/six.md", "too deep");
    t.write("repo/image.png", "binary");
    t.write("outside.md", "outside");
    let cwd = t.0.join("repo");

    let names: Vec<String> = found(&t.0, &cwd, false).into_iter().map(|(p, ..)| p).collect();
    assert_eq!(names, ["repo/CLAUDE.md", "repo/one.md", "repo/two.md", "repo/three.md", "repo/four.md"]);

    let names: Vec<String> = found(&t.0, &cwd, true).into_iter().map(|(p, ..)| p).collect();
    assert!(names.contains(&"outside.md".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n.ends_with(".png")));
}

#[test]
fn reads_the_account_email_from_data() {
    let t = TempDir::new("account");
    let file = t.write(
        "home/.claude.json",
        r#"{"numStartups":3,"userID":"dev1","oauthAccount":{"emailAddress":"someone@example.org","accountUuid":"u"},
           "projects":{"/w/a":{"hasClaudeMdExternalIncludesApproved":true},"/w/b":{"allowedTools":[]}}}"#,
    );
    let config = read_global_config(&file).expect("read");
    assert_eq!(config.email.as_deref(), Some("someone@example.org"));
    assert_eq!(config.account_uuid.as_deref(), Some("u"));
    assert_eq!(config.device_id.as_deref(), Some("dev1"));
    assert!(config.allows_external_imports(Path::new("/w/a")));
    assert!(!config.allows_external_imports(Path::new("/w/b")));

    let missing = read_global_config(&t.0.join("nope.json")).expect("missing is empty");
    assert_eq!(missing.email, None);
    let signed_out = t.write("home/other.json", r#"{"projects":{}}"#);
    assert_eq!(read_global_config(&signed_out).expect("read").email, None);
}
