mod common;

use common::{Env, err, ok};
use eos_agent_core::content_hash;
use serde_json::json;

const NOTE: &str = " (file state is current in your context \u{2014} no need to Read it back)";
const NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
const MODIFIED: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";

#[tokio::test]
async fn edit_requires_a_read_first() {
    let env = Env::new();
    env.write("a.rs", "let x = 1;\n");
    let input = json!({"file_path": env.p("a.rs"), "old_string": "1", "new_string": "2"});
    assert_eq!(err(&env.edit(input.clone()).await), NOT_READ);
    env.read_file("a.rs").await;
    assert_eq!(ok(&env.edit(input).await), format!("The file {} has been updated successfully.{NOTE}", env.p("a.rs")));
    assert_eq!(env.read("a.rs"), "let x = 2;\n");
    let stamp = env.states().get(&env.path("a.rs")).expect("stamp");
    assert_eq!(stamp.hash, content_hash(b"let x = 2;\n"));
    assert!(!stamp.partial);
}

#[tokio::test]
async fn edit_after_a_window_read_is_allowed() {
    let env = Env::new();
    env.write("a.txt", "one\ntwo\nthree\n");
    ok(&env.read_tool(json!({"file_path": env.p("a.txt"), "offset": 2, "limit": 1})).await);
    let out = ok(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "two", "new_string": "2"})).await);
    assert_eq!(out, format!("The file {} has been updated successfully.", env.p("a.txt")));
    assert_eq!(env.read("a.txt"), "one\n2\nthree\n");
}

#[tokio::test]
async fn uniqueness_and_replace_all() {
    let env = Env::new();
    env.write("a.txt", "foo bar foo\n");
    env.read_file("a.txt").await;
    let msg = err(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "foo", "new_string": "baz"})).await);
    assert_eq!(
        msg,
        "Found 2 matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: foo"
    );
    let out = ok(&env
        .edit(json!({"file_path": env.p("a.txt"), "old_string": "foo", "new_string": "$&baz", "replace_all": true}))
        .await);
    assert_eq!(
        out,
        format!("The file {} has been updated. All occurrences were successfully replaced.{NOTE}", env.p("a.txt"))
    );
    assert_eq!(env.read("a.txt"), "$&baz bar $&baz\n");
}

#[tokio::test]
async fn not_found_and_no_change_messages() {
    let env = Env::new();
    env.write("a.txt", "hello\n");
    env.read_file("a.txt").await;
    let msg = err(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "bye", "new_string": "x"})).await);
    assert_eq!(msg, "String to replace not found in file.\nString: bye");
    let msg = err(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "x", "new_string": "x"})).await);
    assert_eq!(msg, "No changes to make: old_string and new_string are exactly the same.");
}

#[tokio::test]
async fn curly_quotes_match_straight_ones_and_keep_their_style() {
    let env = Env::new();
    env.write("q.md", "She said \u{201c}don\u{2019}t\u{201d} twice.\n");
    env.read_file("q.md").await;
    ok(&env
        .edit(json!({"file_path": env.p("q.md"), "old_string": "said \"don't\"", "new_string": "said \"can't\""}))
        .await);
    assert_eq!(env.read("q.md"), "She said \u{201c}can\u{2019}t\u{201d} twice.\n");
}

#[tokio::test]
async fn unicode_escapes_are_swapped() {
    let env = Env::new();
    env.write("s.json", "{\"k\": \"caf\\u00e9\"}\n");
    env.read_file("s.json").await;
    ok(&env.edit(json!({"file_path": env.p("s.json"), "old_string": "caf\u{e9}", "new_string": "th\u{e9}"})).await);
    assert_eq!(env.read("s.json"), "{\"k\": \"th\\u00e9\"}\n");
}

#[tokio::test]
async fn crlf_and_bom_survive_an_edit() {
    let env = Env::new();
    env.write("w.txt", "\u{feff}a\r\nb\r\nc\r\n");
    env.read_file("w.txt").await;
    ok(&env.edit(json!({"file_path": env.p("w.txt"), "old_string": "b\nc", "new_string": "B\nC\nD"})).await);
    assert_eq!(env.read("w.txt"), "\u{feff}a\r\nB\r\nC\r\nD\r\n");
}

#[tokio::test]
async fn utf16_files_keep_their_encoding() {
    let env = Env::new();
    let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
    env.write("u.txt", utf16("\u{feff}h\u{e9}llo\n"));
    env.read_file("u.txt").await;
    ok(&env.edit(json!({"file_path": env.p("u.txt"), "old_string": "h\u{e9}llo", "new_string": "bye"})).await);
    assert_eq!(env.read_bytes("u.txt"), utf16("\u{feff}bye\n"));
}

#[tokio::test]
async fn deleting_a_line_removes_its_newline() {
    let env = Env::new();
    env.write("a.txt", "keep\ndrop\nkeep too\n");
    env.read_file("a.txt").await;
    ok(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "drop", "new_string": ""})).await);
    assert_eq!(env.read("a.txt"), "keep\nkeep too\n");
}

#[tokio::test]
async fn stale_file_refused_unless_the_edit_still_applies() {
    let env = Env::new();
    env.write("a.txt", "alpha\nbeta\n");
    env.read_file("a.txt").await;
    env.write("a.txt", "alpha\nbeta\ngamma\n");
    env.set_mtime("a.txt", 5);
    let msg = err(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "delta", "new_string": "x"})).await);
    assert_eq!(msg, MODIFIED);
    let out = ok(&env.edit(json!({"file_path": env.p("a.txt"), "old_string": "beta", "new_string": "BETA"})).await);
    assert!(out.ends_with("(note: the file had been modified on disk since you last read it \u{2014} the edit applied cleanly, but the file contains other changes not in your context. Read it before edits that depend on surrounding content.)"), "{out}");
    assert_eq!(env.read("a.txt"), "alpha\nBETA\ngamma\n");
}

#[tokio::test]
async fn touched_but_unchanged_file_is_not_stale() {
    let env = Env::new();
    env.write("a.txt", "same\n");
    env.read_file("a.txt").await;
    env.set_mtime("a.txt", 5);
    let out = ok(&env.write_tool(json!({"file_path": env.p("a.txt"), "content": "new\n"})).await);
    assert_eq!(out, format!("The file {} has been updated successfully.{NOTE}", env.p("a.txt")));
}

#[tokio::test]
async fn edit_creates_files_from_an_empty_old_string() {
    let env = Env::new();
    let out = ok(&env.edit(json!({"file_path": env.p("new/b.txt"), "old_string": "", "new_string": "hi\n"})).await);
    assert_eq!(out, format!("The file {} has been updated successfully.{NOTE}", env.p("new/b.txt")));
    assert_eq!(env.read("new/b.txt"), "hi\n");
    let msg = err(&env.edit(json!({"file_path": env.p("new/b.txt"), "old_string": "", "new_string": "x"})).await);
    assert_eq!(msg, "Cannot create new file - file already exists.");
    let msg = err(&env.edit(json!({"file_path": env.p("nope.txt"), "old_string": "a", "new_string": "b"})).await);
    assert!(msg.starts_with("File does not exist. Note: your current working directory is"), "{msg}");
}

#[tokio::test]
async fn notebooks_go_to_notebook_edit() {
    let env = Env::new();
    env.write("n.ipynb", "{\"cells\": []}");
    let msg = err(&env.edit(json!({"file_path": env.p("n.ipynb"), "old_string": "cells", "new_string": "x"})).await);
    assert_eq!(msg, "File is a Jupyter Notebook. Use the NotebookEdit to edit this file.");
}

#[tokio::test]
async fn write_creates_and_overwrites_read_files() {
    let env = Env::new();
    let out = ok(&env.write_tool(json!({"file_path": env.p("deep/dir/f.txt"), "content": "one\n"})).await);
    assert_eq!(out, format!("File created successfully at: {}{NOTE}", env.p("deep/dir/f.txt")));
    let out = ok(&env.write_tool(json!({"file_path": env.p("deep/dir/f.txt"), "content": "two\n"})).await);
    assert_eq!(out, format!("The file {} has been updated successfully.{NOTE}", env.p("deep/dir/f.txt")));
    assert_eq!(env.read("deep/dir/f.txt"), "two\n");

    env.write("other.txt", "x");
    assert_eq!(err(&env.write_tool(json!({"file_path": env.p("other.txt"), "content": "y"})).await), NOT_READ);
    env.read_file("other.txt").await;
    env.write("other.txt", "changed elsewhere");
    env.set_mtime("other.txt", 5);
    assert_eq!(err(&env.write_tool(json!({"file_path": env.p("other.txt"), "content": "y"})).await), MODIFIED);

    std::fs::create_dir(env.path("adir")).expect("mkdir");
    let msg = err(&env.write_tool(json!({"file_path": env.p("adir"), "content": "y"})).await);
    assert!(
        msg.ends_with("is a directory, not a file. To create a file inside it, include the file name in file_path.")
    );
}

#[tokio::test]
async fn write_keeps_permissions_and_leaves_no_temp_files() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.write("run.sh", "#!/bin/sh\n");
    std::fs::set_permissions(env.path("run.sh"), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    env.read_file("run.sh").await;
    ok(&env.write_tool(json!({"file_path": env.p("run.sh"), "content": "#!/bin/sh\necho hi\n"})).await);
    let mode = std::fs::metadata(env.path("run.sh")).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    let names: Vec<_> = std::fs::read_dir(&env.dir).expect("ls").flatten().map(|e| e.file_name()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[tokio::test]
async fn edits_and_writes_tell_the_host_what_changed() {
    let env = Env::new();
    env.write("a.txt", "one\ntwo\nthree\nfour\nfive\n");
    env.read_file("a.txt").await;
    let out = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "three", "new_string": "3"})).await;
    let details: serde_json::Value = serde_json::from_str(out.details.as_ref().expect("details").get()).expect("json");
    assert_eq!(
        details,
        json!({
            "filePath": env.p("a.txt"),
            "oldString": "three",
            "newString": "3",
            "replaceAll": false,
            "structuredPatch": [{
                "oldStart": 1, "oldLines": 5, "newStart": 1, "newLines": 5,
                "lines": [" one", " two", "-three", "+3", " four", " five"],
            }],
        })
    );

    // Overwriting gives the same hunks; a new file has none.
    let out = env.write_tool(json!({"file_path": env.p("a.txt"), "content": "one\ntwo\n3\nfour\n"})).await;
    let details: serde_json::Value = serde_json::from_str(out.details.as_ref().expect("details").get()).expect("json");
    assert_eq!(details["type"], "update");
    assert_eq!(details["structuredPatch"][0]["lines"], json!([" two", " 3", " four", "-five"]));
    let out = env.write_tool(json!({"file_path": env.p("new.txt"), "content": "hi\n"})).await;
    let details: serde_json::Value = serde_json::from_str(out.details.as_ref().expect("details").get()).expect("json");
    assert_eq!(details, json!({"type": "create", "filePath": env.p("new.txt"), "structuredPatch": []}));
    // A refused edit has nothing to tell.
    let refused = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "nope", "new_string": "x"})).await;
    assert!(refused.is_error && refused.details.is_none());
}
