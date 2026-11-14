//! Tools that only read take any path the process can read (the permission
//! gate decides); tools that write stay inside the working directories.

mod common;

use common::{Env, err, ok};
use serde_json::json;

const WALL: &str = "is outside this agent's working directories.";

#[tokio::test]
async fn read_takes_text_and_images_outside_the_roots() {
    let env = Env::new();
    let elsewhere = Env::new();
    elsewhere.write("notes.txt", "far away\n");
    elsewhere.write("paste/image.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");

    let out = ok(&env.read_tool(json!({"file_path": elsewhere.p("notes.txt")})).await);
    assert!(out.starts_with("1\tfar away"), "{out}");
    let out = ok(&env.read_tool(json!({"file_path": elsewhere.p("paste/image.png")})).await);
    let blocks: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(blocks[0]["type"], "image");
    assert_eq!(blocks[0]["source"]["media_type"], "image/png");

    let relative = format!("../{}/notes.txt", elsewhere.dir.file_name().expect("name").to_string_lossy());
    let out = ok(&env.read_tool(json!({"file_path": relative})).await);
    assert!(out.starts_with("1\tfar away"), "{out}");
}

#[tokio::test]
async fn glob_and_grep_search_outside_the_roots() {
    let env = Env::new();
    let elsewhere = Env::new();
    elsewhere.write("src/a.rs", "fn needle() {}\n");

    let out = ok(&env.glob(json!({"pattern": "**/*.rs", "path": elsewhere.p("src")})).await);
    assert!(out.ends_with("src/a.rs"), "{out}");
    let out = ok(&env.glob(json!({"pattern": format!("{}/**/*.rs", elsewhere.p("src"))})).await);
    assert!(out.ends_with("src/a.rs"), "{out}");
    let out = ok(&env.grep(json!({"pattern": "needle", "path": elsewhere.p("src")})).await);
    assert!(out.contains("a.rs"), "{out}");
    let out =
        ok(&env.grep(json!({"pattern": "needle", "path": elsewhere.p("src/a.rs"), "output_mode": "content"})).await);
    assert!(out.contains("fn needle()"), "{out}");
}

#[tokio::test]
async fn writes_outside_the_roots_are_still_refused() {
    let env = Env::new();
    let elsewhere = Env::new();
    elsewhere.write("notes.txt", "far away\n");
    ok(&env.read_tool(json!({"file_path": elsewhere.p("notes.txt")})).await);

    let msg = err(&env.write_tool(json!({"file_path": elsewhere.p("new.txt"), "content": "x"})).await);
    assert_eq!(msg, format!("{} {WALL}", elsewhere.p("new.txt")));
    let msg =
        err(&env.edit(json!({"file_path": elsewhere.p("notes.txt"), "old_string": "far", "new_string": "near"})).await);
    assert_eq!(msg, format!("{} {WALL}", elsewhere.p("notes.txt")));
    let msg = err(
        &env.notebook(json!({"notebook_path": elsewhere.p("n.ipynb"), "new_source": "x", "edit_mode": "insert", "cell_type": "code"}))
            .await,
    );
    assert_eq!(msg, format!("{} {WALL}", elsewhere.p("n.ipynb")));
    assert!(!elsewhere.path("new.txt").exists());
    assert_eq!(elsewhere.read("notes.txt"), "far away\n");
}
