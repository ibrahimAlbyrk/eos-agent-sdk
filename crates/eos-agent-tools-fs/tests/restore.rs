//! Rebuilding what a resumed agent had read from its transcript.

mod common;

use common::{Env, err, ok, text};
use eos_agent_core::{Extensions, StoredMessage, Tool, Workspace};
use eos_agent_tools_fs::ReadTool;
use eos_agent_types::{ContentBlock, Role, ToolResultContent};
use serde_json::{Value, json};

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_millis() as u64
}

/// A conversation holding one call of `tool` and its result, recorded at `at_ms`.
fn history(tool: &str, input: Value, result: &str, at_ms: Option<u64>) -> Vec<StoredMessage> {
    let call = ContentBlock::ToolUse {
        id: "toolu_1".into(),
        name: tool.into(),
        input: serde_json::value::to_raw_value(&input).expect("raw"),
    };
    let answer = ContentBlock::ToolResult {
        tool_use_id: "toolu_1".into(),
        content: Some(ToolResultContent::Text(result.into())),
        is_error: false,
    };
    vec![
        StoredMessage::user_text("look at it"),
        StoredMessage::new(Role::Assistant, &[call]).expect("call"),
        StoredMessage::new(Role::User, &[answer]).expect("result").with_recorded_ms(at_ms),
    ]
}

/// The agent's state restarted with only `history` to go on.
async fn resume(env: &mut Env, history: &[StoredMessage]) {
    env.ext = Extensions::default();
    env.ext.insert(Workspace::new(env.dir.clone(), vec![], env.dir.join(".scratch")));
    ReadTool::new().restore(history, &env.ext).await;
}

/// Reads `rel` for real and returns the conversation that recorded it.
async fn read_history(env: &Env, input: Value) -> Vec<StoredMessage> {
    let out = env.read_tool(input.clone()).await;
    history("Read", input, &ok(&out), Some(now_ms()))
}

#[tokio::test]
async fn an_unchanged_file_read_before_may_be_edited() {
    let mut env = Env::new();
    env.write("a.txt", "alpha\nbeta\n");
    let past = read_history(&env, json!({"file_path": env.p("a.txt")})).await;
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "beta", "new_string": "gamma"})).await;
    assert!(ok(&out).contains("has been updated successfully"));
    assert_eq!(env.read("a.txt"), "alpha\ngamma\n");
}

#[tokio::test]
async fn a_file_changed_since_must_be_read_again() {
    let mut env = Env::new();
    env.write("a.txt", "alpha\nbeta\n");
    let past = read_history(&env, json!({"file_path": env.p("a.txt")})).await;
    env.write("a.txt", "alpha\ndelta\n");
    env.set_mtime("a.txt", 5);
    resume(&mut env, &past).await;
    let edit = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "beta", "new_string": "gamma"})).await;
    assert!(err(&edit).contains("modified since read"), "{}", text(&edit));
    let write = env.write_tool(json!({"file_path": env.p("a.txt"), "content": "new"})).await;
    assert!(err(&write).contains("modified since read"), "{}", text(&write));
    assert_eq!(env.read("a.txt"), "alpha\ndelta\n");
}

#[tokio::test]
async fn a_touched_file_with_the_same_text_still_counts_as_read() {
    let mut env = Env::new();
    env.write("a.txt", "alpha\nbeta\n");
    let past = read_history(&env, json!({"file_path": env.p("a.txt")})).await;
    env.set_mtime("a.txt", 5);
    resume(&mut env, &past).await;
    let out = env.write_tool(json!({"file_path": env.p("a.txt"), "content": "new\n"})).await;
    assert!(!out.is_error, "{}", text(&out));
}

#[tokio::test]
async fn writes_and_edits_count_and_files_never_seen_do_not() {
    let mut env = Env::new();
    env.write("w.txt", "written\n");
    env.write("other.txt", "x\n");
    let past = history("Write", json!({"file_path": env.p("w.txt"), "content": "written\n"}), "ok", Some(now_ms()));
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("w.txt"), "old_string": "written", "new_string": "edited"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let other = env.edit(json!({"file_path": env.p("other.txt"), "old_string": "x", "new_string": "y"})).await;
    assert!(err(&other).contains("has not been read yet"));

    // An Edit leaves the file as the agent made it, whatever the time.
    env.write("e.txt", "made by the edit\n");
    env.set_mtime("e.txt", 5);
    let past = history("Edit", json!({"file_path": env.p("e.txt"), "old_string": "a", "new_string": "b"}), "ok", None);
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("e.txt"), "old_string": "made", "new_string": "done"})).await;
    assert!(!out.is_error, "{}", text(&out));
}

#[tokio::test]
async fn transcripts_without_times_compare_the_text() {
    let mut env = Env::new();
    env.write("a.txt", "alpha\nbeta\n");
    let input = json!({"file_path": env.p("a.txt")});
    let shown = ok(&env.read_tool(input.clone()).await);
    let past = history("Read", input.clone(), &shown, None);
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "beta", "new_string": "gamma"})).await;
    assert!(!out.is_error, "the same text: {}", text(&out));

    resume(&mut env, &past).await;
    let out = env.write_tool(json!({"file_path": env.p("a.txt"), "content": "x"})).await;
    assert!(err(&out).contains("modified since read"), "the text changed: {}", text(&out));
}

#[tokio::test]
async fn a_range_read_is_partial_and_a_cut_read_must_be_redone() {
    let mut env = Env::new();
    env.write("a.txt", "one\ntwo\nthree\n");
    let ranged = json!({"file_path": env.p("a.txt"), "offset": 2, "limit": 1});
    let past = read_history(&env, ranged).await;
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "two", "new_string": "2"})).await;
    assert!(!out.is_error, "a ranged read allows edits: {}", text(&out));

    let cut = "[Truncated: PARTIAL view \u{2014} a.txt: showing lines 1-1 of 3 total]\n\n1\tone";
    let past = history("Read", json!({"file_path": env.p("a.txt")}), cut, Some(now_ms()));
    resume(&mut env, &past).await;
    let out = env.edit(json!({"file_path": env.p("a.txt"), "old_string": "one", "new_string": "1"})).await;
    assert!(err(&out).contains("has not been read yet"), "{}", text(&out));
}
