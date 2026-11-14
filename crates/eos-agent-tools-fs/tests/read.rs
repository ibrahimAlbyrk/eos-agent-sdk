mod common;

use common::{Env, err, ok};
use eos_agent_core::{Tool, content_hash};
use eos_agent_tools_fs::{ReadLimits, ReadTool, tools};
use serde_json::json;

#[tokio::test]
async fn numbers_lines_without_padding() {
    let env = Env::new();
    env.write("a.txt", "alpha\n\tbeta\n");
    let out = ok(&env.read_tool(json!({"file_path": env.p("a.txt")})).await);
    assert_eq!(out, "1\talpha\n2\t\tbeta\n3\t");
}

#[tokio::test]
async fn offset_and_limit_pick_a_window() {
    let env = Env::new();
    env.write("a.txt", "l1\nl2\nl3\nl4\nl5");
    let out = ok(&env.read_tool(json!({"file_path": env.p("a.txt"), "offset": 2, "limit": 2})).await);
    assert_eq!(out, "2\tl2\n3\tl3");
    let out = ok(&env.read_tool(json!({"file_path": env.p("a.txt"), "offset": "4", "length": 10})).await);
    assert_eq!(out, "4\tl4\n5\tl5");
    let stamp = env.states().get(&env.path("a.txt")).expect("stamp");
    assert!(stamp.partial);
    assert_eq!(stamp.hash, content_hash(b"l1\nl2\nl3\nl4\nl5"));
}

#[tokio::test]
async fn empty_and_short_files_warn() {
    let env = Env::new();
    env.write("empty.txt", "");
    env.write("short.txt", "a\nb");
    let out = ok(&env.read_tool(json!({"file_path": env.p("empty.txt")})).await);
    assert_eq!(out, "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>");
    let out = ok(&env.read_tool(json!({"file_path": env.p("short.txt"), "offset": 9})).await);
    assert_eq!(
        out,
        "<system-reminder>Warning: the file exists but is shorter than the provided offset (9). The file has 2 lines.</system-reminder>"
    );
}

#[tokio::test]
async fn missing_file_suggests_a_sibling() {
    let env = Env::new();
    env.write("main.rs", "fn main() {}");
    let msg = err(&env.read_tool(json!({"file_path": env.p("main.py")})).await);
    assert_eq!(
        msg,
        format!(
            "File does not exist. Note: your current working directory is {}. Did you mean main.rs?",
            env.dir.display()
        )
    );
}

#[tokio::test]
async fn crlf_and_bom_are_hidden() {
    let env = Env::new();
    env.write("w.txt", "\u{feff}one\r\ntwo\r\n");
    assert_eq!(ok(&env.read_tool(json!({"file_path": env.p("w.txt")})).await), "1\tone\n2\ttwo\n3\t");
}

#[tokio::test]
async fn whole_file_over_the_byte_cap_is_refused() {
    let env = Env::new();
    env.write("big.txt", "x\n".repeat(150_000));
    let msg = err(&env.read_tool(json!({"file_path": env.p("big.txt")})).await);
    assert!(msg.starts_with("File content (293KB) exceeds maximum allowed size (256KB)."), "{msg}");
    assert!(msg.contains("call Read again with allow_large: true"), "{msg}");
    let out = ok(&env.read_tool(json!({"file_path": env.p("big.txt"), "offset": 100, "limit": 2})).await);
    assert_eq!(out, "100\tx\n101\tx");
}

fn long_lines() -> String {
    (0..2000).map(|i| format!("{i:04} {}\n", "word ".repeat(18))).collect()
}

#[tokio::test]
async fn over_the_token_cap_pages_with_a_banner() {
    let env = Env::new();
    env.write("doc.txt", long_lines());
    let out = ok(&env.read_tool(json!({"file_path": env.p("doc.txt")})).await);
    let banner = out.lines().next().expect("banner");
    assert!(
        banner.starts_with(&format!("[Truncated: PARTIAL view \u{2014} {}: showing lines 1-", env.p("doc.txt"))),
        "{banner}"
    );
    assert!(banner.contains("of 2001 total ("), "{banner}");
    assert!(banner.contains("tokens, cap 25000). Call Read with offset="), "{banner}");
    assert!(banner.ends_with("Do NOT answer from this page alone if the answer may be further in the file.]"));
    assert!(out.contains("\n\n1\t0000 word"));
    assert!(out.len() / 4 < 26_000);

    let msg = err(&env.edit(json!({"file_path": env.p("doc.txt"), "old_string": "0001", "new_string": "x"})).await);
    assert_eq!(msg, "File has not been read yet. Read it first before writing to it.");
    let msg = err(&env.write_tool(json!({"file_path": env.p("doc.txt"), "content": "x"})).await);
    assert!(msg.starts_with("You were given only the start of this file"), "{msg}");
}

#[tokio::test]
async fn allow_large_reads_past_the_cap() {
    let env = Env::new();
    env.write("doc.txt", long_lines());
    let out = ok(&env.read_tool(json!({"file_path": env.p("doc.txt"), "allow_large": true})).await);
    assert!(out.starts_with("1\t0000"));
    assert!(out.contains("\n2000\t1999 "));
    ok(&env.edit(json!({"file_path": env.p("doc.txt"), "old_string": "0001 word", "new_string": "one"})).await);

    env.ext.insert(ReadLimits { large_read_room: None, ..ReadLimits::default() });
    let out = ok(&env.read_tool(json!({"file_path": env.p("doc.txt"), "allow_large": true})).await);
    assert!(out.starts_with("[Truncated: PARTIAL view"));
    assert!(out.contains("allow_large is not available where this read runs"));
}

#[tokio::test]
async fn window_over_the_token_cap_is_an_error() {
    let env = Env::new();
    env.write("doc.txt", long_lines());
    let msg = err(&env.read_tool(json!({"file_path": env.p("doc.txt"), "offset": 1, "limit": 1900})).await);
    assert!(msg.starts_with("File content ("), "{msg}");
    assert!(msg.contains("tokens) exceeds maximum allowed tokens (25000)."));
}

#[tokio::test]
async fn binary_files_and_directories_are_refused() {
    let env = Env::new();
    env.write("a.zip", "PK");
    let msg = err(&env.read_tool(json!({"file_path": env.p("a.zip")})).await);
    assert!(msg.starts_with("This tool cannot read binary files. The file appears to be a binary .zip file."));
    std::fs::create_dir(env.path("dir")).expect("mkdir");
    let msg = err(&env.read_tool(json!({"file_path": env.p("dir")})).await);
    assert!(msg.starts_with("EISDIR: illegal operation on a directory"));
}

#[tokio::test]
async fn not_utf8_gets_a_note() {
    let env = Env::new();
    env.write("latin1.txt", b"caf\xe9\n");
    let out = ok(&env.read_tool(json!({"file_path": env.p("latin1.txt")})).await);
    assert!(out.starts_with("1\tcaf\u{fffd}\n2\t\n\n[This file is not valid UTF-8"), "{out}");
}

#[tokio::test]
async fn images_become_image_blocks() {
    let env = Env::new();
    env.write("dot.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    let out = ok(&env.read_tool(json!({"file_path": env.p("dot.png")})).await);
    let blocks: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(blocks[0]["type"], "image");
    assert_eq!(blocks[0]["source"]["media_type"], "image/png");
    assert_eq!(blocks[0]["source"]["data"], "iVBORw0KGgoAAAANSUhEUg==");
    env.write("fake.jpg", "<html>login</html>");
    let msg = err(&env.read_tool(json!({"file_path": env.p("fake.jpg")})).await);
    assert!(msg.contains("Detected: an HTML page."), "{msg}");
}

#[tokio::test]
async fn pdfs_become_document_blocks() {
    let env = Env::new();
    env.write("two.pdf", "%PDF-1.4\n<< /Type /Page >>\n<< /Type /Page >>\n%%EOF");
    let out = ok(&env.read_tool(json!({"file_path": env.p("two.pdf")})).await);
    let blocks: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(blocks[0]["text"], format!("PDF file read: {} (50 bytes)", env.p("two.pdf")));
    assert_eq!(blocks[1]["type"], "document");
    assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");

    env.write("many.pdf", format!("%PDF-1.4\n{}", "<< /Type /Page >>\n".repeat(11)));
    let msg = err(&env.read_tool(json!({"file_path": env.p("many.pdf")})).await);
    assert!(msg.starts_with("This PDF has 11 pages, which is too many to read at once."), "{msg}");
    let msg = err(&env.read_tool(json!({"file_path": env.p("many.pdf"), "pages": "1,2"})).await);
    assert!(msg.starts_with("Invalid pages parameter"));
}

#[tokio::test]
async fn notebooks_render_cells() {
    let env = Env::new();
    let nb = json!({"cells": [
        {"cell_type": "code", "id": "c1", "source": ["x = 1\n", "x"], "outputs": [
            {"output_type": "execute_result", "data": {"text/plain": ["1"]}}]},
        {"cell_type": "markdown", "id": "m1", "source": "# Hi"}
    ], "metadata": {"language_info": {"name": "python"}}, "nbformat": 4, "nbformat_minor": 5});
    env.write("n.ipynb", nb.to_string());
    let out = ok(&env.read_tool(json!({"file_path": env.p("n.ipynb")})).await);
    let blocks: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        blocks[0]["text"],
        "<cell id=\"c1\">x = 1\nx</cell id=\"c1\">\n\n1\n<cell id=\"m1\"><cell_type>markdown</cell_type># Hi</cell id=\"m1\">"
    );
}

#[test]
fn specs_match_claude_code() {
    let names: Vec<String> = tools().iter().map(|t| t.spec().name().to_owned()).collect();
    assert_eq!(names, ["Read", "Write", "Edit", "Glob", "Grep", "NotebookEdit"]);
    let read = ReadTool::new();
    let spec: serde_json::Value = serde_json::from_slice(read.spec().json()).expect("json");
    assert!(spec["description"].as_str().expect("desc").starts_with("Reads a file from the local filesystem."));
    assert_eq!(spec["input_schema"]["required"], json!(["file_path"]));
    let toolbox = eos_agent_core::Toolbox::new(tools());
    let defs: serde_json::Value = serde_json::from_slice(toolbox.json().expect("tools")).expect("json");
    assert_eq!(defs[4]["input_schema"]["properties"]["head_limit"]["type"], "number");
    assert!(read.is_concurrency_safe(&serde_json::value::RawValue::from_string("{}".into()).expect("raw")));
}

#[tokio::test]
async fn unknown_fields_are_rejected() {
    let env = Env::new();
    env.write("a.txt", "x");
    let msg = err(&env.read_tool(json!({"file_path": env.p("a.txt"), "bogus": 1})).await);
    assert!(msg.starts_with("InputValidationError: unknown field `bogus`"), "{msg}");
    ok(&env.read_tool(json!({"file_path": env.p("a.txt"), "description": "why"})).await);
}

#[tokio::test]
async fn a_cancelled_read_stops() {
    let env = Env::new();
    env.write("a.txt", "x");
    env.cancel.cancel();
    assert_eq!(err(&env.read_tool(json!({"file_path": env.p("a.txt")})).await), "The read was cancelled.");
}
