mod common;

use common::{Env, err, ok};
use serde_json::json;

#[tokio::test]
async fn glob_sorts_oldest_first_and_sees_hidden_and_ignored_files() {
    let env = Env::new();
    env.write(".gitignore", "ignored.rs\n");
    std::fs::create_dir(env.path(".git")).expect("git dir");
    for (name, age) in [("src/new.rs", 10), ("src/old.rs", 300), (".hidden/h.rs", 100), ("ignored.rs", 200)] {
        env.write(name, "x");
        env.set_mtime(name, -age);
    }
    env.write("notes.txt", "x");
    let out = ok(&env.glob(json!({"pattern": "*.rs"})).await);
    assert_eq!(out, "src/old.rs\nignored.rs\n.hidden/h.rs\nsrc/new.rs");
    let out = ok(&env.glob(json!({"pattern": "*.rs", "path": env.p("src")})).await);
    assert_eq!(out, "src/old.rs\nsrc/new.rs");
    let out = ok(&env.glob(json!({"pattern": "src/*.rs", "path": env.p("src")})).await);
    assert_eq!(out, "src/old.rs\nsrc/new.rs");
    assert_eq!(ok(&env.glob(json!({"pattern": "**/*.go"})).await), "No files found");
}

#[tokio::test]
async fn glob_caps_at_one_hundred() {
    let env = Env::new();
    for i in 0..120 {
        env.write(&format!("f{i:03}.txt"), "x");
    }
    let out = ok(&env.glob(json!({"pattern": "*.txt"})).await);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 101);
    assert_eq!(
        lines[100],
        "(Showing 100 of 120 matching files; 20 more are not listed. Narrow the pattern or path to see the rest.)"
    );
}

#[tokio::test]
async fn glob_rejects_bad_directories() {
    let env = Env::new();
    env.write("file.txt", "x");
    let msg = err(&env.glob(json!({"pattern": "*", "path": env.p("nope")})).await);
    assert!(
        msg.starts_with(&format!(
            "Directory does not exist: {}. Note: your current working directory is",
            env.p("nope")
        ))
    );
    let msg = err(&env.glob(json!({"pattern": "*", "path": env.p("file.txt")})).await);
    assert_eq!(msg, format!("Path is not a directory: {}", env.p("file.txt")));
}

fn grep_fixture(env: &Env) {
    std::fs::create_dir(env.path(".git")).expect("git dir");
    env.write(".git/config", "needle in vcs");
    env.write(".gitignore", "build/\n");
    env.write("build/out.txt", "needle ignored");
    env.write(".env", "NEEDLE=hidden");
    env.write("a.rs", "fn a() {}\n// needle one\nfn b() {}\n");
    env.write("b.py", "needle = 2\nother\nneedle again\n");
    env.set_mtime("a.rs", -100);
    env.set_mtime(".env", -50);
}

#[tokio::test]
async fn grep_lists_files_newest_first() {
    let env = Env::new();
    grep_fixture(&env);
    let out = ok(&env.grep(json!({"pattern": "needle", "-i": true})).await);
    assert_eq!(out, "Found 3 files\nb.py\n.env\na.rs");
    let out = ok(&env.grep(json!({"pattern": "needle"})).await);
    assert_eq!(out, "Found 2 files\nb.py\na.rs");
    assert_eq!(ok(&env.grep(json!({"pattern": "absent"})).await), "No files found");
}

#[tokio::test]
async fn grep_content_and_count_modes() {
    let env = Env::new();
    grep_fixture(&env);
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "content"})).await);
    assert_eq!(out, "a.rs:2:// needle one\nb.py:1:needle = 2\nb.py:3:needle again");
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "content", "-n": false, "type": "py"})).await);
    assert_eq!(out, "b.py:needle = 2\nb.py:needle again");
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "content", "-C": 1, "glob": "*.py"})).await);
    assert_eq!(out, "b.py:1:needle = 2\nb.py-2-other\nb.py:3:needle again");
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "content", "-A": 0, "-B": 1})).await);
    assert_eq!(out, "a.rs-1-fn a() {}\na.rs:2:// needle one\n--\nb.py:1:needle = 2\nb.py-2-other\nb.py:3:needle again");
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "count"})).await);
    assert_eq!(out, "a.rs:1\nb.py:2\n\nFound 3 total occurrences across 2 files.");
    let out =
        ok(&env.grep(json!({"pattern": "ne+dle", "output_mode": "content", "-o": true, "path": env.p("b.py")})).await);
    assert_eq!(out, "1:needle\n3:needle");
}

#[tokio::test]
async fn grep_paginates() {
    let env = Env::new();
    env.write("many.txt", (0..300).map(|i| format!("hit {i}\n")).collect::<String>());
    let out = ok(&env.grep(json!({"pattern": "hit", "output_mode": "content"})).await);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 252);
    assert_eq!(lines[249], "many.txt:250:hit 249");
    assert_eq!(lines[251], "[Showing results with pagination = limit: 250]");
    let out = ok(&env.grep(json!({"pattern": "hit", "output_mode": "content", "head_limit": 2, "offset": 10})).await);
    assert_eq!(
        out,
        "many.txt:11:hit 10\nmany.txt:12:hit 11\n\n[Showing results with pagination = limit: 2, offset: 10]"
    );
    let out = ok(&env.grep(json!({"pattern": "hit", "output_mode": "content", "offset": 500})).await);
    assert_eq!(out, "No entries at this offset\n\n[Showing results with pagination = offset: 500]");
    let out = ok(&env.grep(json!({"pattern": "hit", "output_mode": "content", "head_limit": 0, "offset": 298})).await);
    assert_eq!(out, "many.txt:299:hit 298\nmany.txt:300:hit 299\n\n[Showing results with pagination = offset: 298]");
}

#[tokio::test]
async fn grep_multiline_long_lines_and_input_fixes() {
    let env = Env::new();
    env.write("s.go", "type T struct {\n  a int\n  field string\n}\n");
    env.write("long.txt", format!("needle {}\n", "x".repeat(600)));
    let out = ok(&env
        .grep(json!({"pattern": "struct \\{[\\s\\S]*?field", "multiline": true, "output_mode": "content"}))
        .await);
    assert_eq!(out, "s.go:1:type T struct {\ns.go:2:  a int\ns.go:3:  field string");
    let out = ok(&env.grep(json!({"pattern": "needle", "output_mode": "content"})).await);
    assert_eq!(out, "long.txt:1:[Omitted long matching line]");
    let out = ok(&env.grep(json!({"pattern": "needle", "file_path": env.p("long.txt"), "-c": true})).await);
    assert_eq!(
        out,
        "long.txt:1\n\nFound 1 total occurrence across 1 file.\n\nNote: Grep's parameter for where to search is named `path`. `file_path` was read as `path`. `-c` is not a Grep parameter and was read as `output_mode: \"count\"`."
    );
    let msg = err(&env.grep(json!({"pattern": "x", "head_limit": 1.5})).await);
    assert_eq!(msg, "head_limit must be a whole number of 0 or more, got 1.5. Pass 0 for unlimited.");
    let msg = err(&env.grep(json!({"pattern": "x", "path": env.p("missing")})).await);
    assert!(msg.starts_with(&format!("Path does not exist: {}.", env.p("missing"))));
}
