//! `cargo xtask parity [THEIRS OURS]`: compares one request Claude Code sent
//! with one this engine sent (both `request.json` from `tools/capture-proxy`).
//! Without arguments it takes the newest capture of each in
//! `fixtures/captured`. It reports top-level body fields, headers and betas,
//! system blocks byte for byte, tools (order, deferral, descriptions,
//! schemas), the first message's context and the system note.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value};

const CAPTURES: &str = "fixtures/captured";
/// Context shown around a difference, and the longest line printed.
const AROUND: usize = 70;
const WIDTH: usize = 150;

struct Request {
    path: PathBuf,
    headers: Map<String, Value>,
    body: Map<String, Value>,
}

pub(crate) fn run(args: &[String]) -> anyhow::Result<()> {
    let (theirs, ours) = match args {
        [] => (latest("claude-cli")?, latest("eos-agent")?),
        [a, b] => (request_file(Path::new(a)), request_file(Path::new(b))),
        _ => bail!("usage: cargo xtask parity [THEIRS OURS]"),
    };
    let (theirs, ours) = (load(&theirs)?, load(&ours)?);
    println!("theirs: {}\nours:   {}\n", theirs.path.display(), ours.path.display());
    fields(&theirs, &ours);
    headers(&theirs, &ours);
    system(&theirs, &ours);
    tools(&theirs, &ours);
    context(&theirs, &ours);
    Ok(())
}

fn request_file(path: &Path) -> PathBuf {
    if path.is_dir() { path.join("request.json") } else { path.to_path_buf() }
}

/// The newest `/v1/messages` capture whose user agent starts with `agent`.
fn latest(agent: &str) -> anyhow::Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(CAPTURES)
        .with_context(|| format!("reading {CAPTURES}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with("v1_messages"))
        .collect();
    dirs.sort();
    dirs.into_iter()
        .rev()
        .map(|d| d.join("request.json"))
        .find(|p| load(p).is_ok_and(|r| header(&r, "user-agent").starts_with(agent)))
        .with_context(|| format!("no capture from {agent} in {CAPTURES}"))
}

fn load(path: &Path) -> anyhow::Result<Request> {
    let doc: Value =
        serde_json::from_slice(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)?;
    let object = |key: &str| doc.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
    Ok(Request { path: path.to_path_buf(), headers: object("headers"), body: object("body") })
}

fn header(r: &Request, name: &str) -> String {
    r.headers.get(name).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn fields(theirs: &Request, ours: &Request) {
    println!("## Top-level fields");
    let mut keys: Vec<&String> = theirs.body.keys().chain(ours.body.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys.into_iter().filter(|k| !matches!(k.as_str(), "system" | "tools" | "messages")) {
        match (theirs.body.get(key), ours.body.get(key)) {
            (Some(a), Some(b)) if a == b => println!("  = {key}"),
            (Some(a), Some(b)) => {
                println!("  ≠ {key}: theirs {} | ours {}", short(&a.to_string()), short(&b.to_string()))
            }
            (Some(a), None) => println!("  - {key} (theirs only): {}", short(&a.to_string())),
            (None, _) => println!("  + {key} (ours only)"),
        }
    }
}

fn headers(theirs: &Request, ours: &Request) {
    println!("\n## Headers");
    let names = |r: &Request| r.headers.keys().cloned().collect::<Vec<_>>();
    let (a, b) = (names(theirs), names(ours));
    println!("  theirs only: {}", missing(&a, &b).join(", "));
    println!("  ours only:   {}", missing(&b, &a).join(", "));
    for name in ["user-agent", "anthropic-version", "content-type", "accept"] {
        let (x, y) = (header(theirs, name), header(ours, name));
        println!(
            "  {} {name}: {x}{}",
            if x == y { "=" } else { "≠" },
            if x == y { String::new() } else { format!(" | {y}") }
        );
    }
    let betas = |r: &Request| header(r, "anthropic-beta").split(',').map(str::to_owned).collect::<Vec<_>>();
    let (a, b) = (betas(theirs), betas(ours));
    println!("  betas theirs only: {}", missing(&a, &b).join(", "));
    println!("  betas ours only:   {}", missing(&b, &a).join(", "));
}

fn system(theirs: &Request, ours: &Request) {
    println!("\n## System blocks");
    let blocks = |r: &Request| r.body.get("system").and_then(Value::as_array).cloned().unwrap_or_default();
    let (a, b) = (blocks(theirs), blocks(ours));
    println!("  count: theirs {} | ours {}", a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let (tx, ty) = (text(x), text(y));
        let cache = if x.get("cache_control") == y.get("cache_control") { "" } else { " (cache_control differs)" };
        match first_difference(tx, ty) {
            None => println!("  [{i}] identical, {} bytes{cache}", tx.len()),
            Some(at) => {
                println!("  [{i}] differ at byte {at} ({} vs {} bytes){cache}", tx.len(), ty.len());
                println!("      theirs: {}", around(tx, at));
                println!("      ours:   {}", around(ty, at));
            }
        }
    }
}

fn tools(theirs: &Request, ours: &Request) {
    println!("\n## Tools");
    let list = |r: &Request| r.body.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    let (a, b) = (list(theirs), list(ours));
    let name = |t: &Value| t.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let deferred = |t: &Value| if t.get("defer_loading") == Some(&Value::Bool(true)) { "*" } else { "" };
    let row = |l: &[Value]| l.iter().map(|t| format!("{}{}", name(t), deferred(t))).collect::<Vec<_>>().join(" ");
    println!("  theirs (* deferred): {}", row(&a));
    println!("  ours   (* deferred): {}", row(&b));
    let (na, nb): (Vec<String>, Vec<String>) = (a.iter().map(name).collect(), b.iter().map(name).collect());
    println!("  theirs only: {}", missing(&na, &nb).join(", "));
    println!("  ours only:   {}", missing(&nb, &na).join(", "));
    let common: Vec<&String> = na.iter().filter(|n| nb.contains(n)).collect();
    let in_b: Vec<&String> = nb.iter().filter(|n| na.contains(n)).collect();
    println!("  common tools in the same order: {}", common == in_b);
    for tool in &a {
        let Some(other) = b.iter().find(|t| name(t) == name(tool)) else { continue };
        let mut notes = Vec::new();
        if deferred(tool) != deferred(other) {
            notes.push("deferral".to_owned());
        }
        let (da, db) = (str_of(tool, "description"), str_of(other, "description"));
        if let Some(at) = first_difference(da, db) {
            notes.push(format!("description at byte {at}: theirs {} | ours {}", around(da, at), around(db, at)));
        }
        if tool.get("input_schema") != other.get("input_schema") {
            notes.push(schema_difference(tool.get("input_schema"), other.get("input_schema")));
        }
        let extra: Vec<&String> =
            tool.as_object().into_iter().flat_map(|o| o.keys()).filter(|k| other.get(k.as_str()).is_none()).collect();
        if !extra.is_empty() {
            notes.push(format!("theirs only keys {extra:?}"));
        }
        let status = if notes.is_empty() { "=".to_owned() } else { format!("≠ {}", notes.join("; ")) };
        println!("  {} {status}", name(tool));
    }
}

fn schema_difference(a: Option<&Value>, b: Option<&Value>) -> String {
    let props = |v: Option<&Value>| {
        v.and_then(|s| s.get("properties")).and_then(Value::as_object).map(|p| p.keys().cloned().collect::<Vec<_>>())
    };
    let (pa, pb) = (props(a).unwrap_or_default(), props(b).unwrap_or_default());
    let (only_a, only_b) = (missing(&pa, &pb), missing(&pb, &pa));
    if only_a.is_empty() && only_b.is_empty() {
        "schema differs in details".to_owned()
    } else {
        format!("schema properties theirs only {only_a:?}, ours only {only_b:?}")
    }
}

fn context(theirs: &Request, ours: &Request) {
    let messages = |r: &Request| r.body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let (a, b) = (messages(theirs), messages(ours));
    let blocks =
        |m: Option<&Value>| m.and_then(|m| m.get("content")).and_then(Value::as_array).cloned().unwrap_or_default();
    println!("\n## First message");
    let (fa, fb) = (blocks(a.first()), blocks(b.first()));
    println!("  blocks: theirs {} | ours {}", fa.len(), fb.len());
    for (i, (x, y)) in fa.iter().zip(&fb).enumerate() {
        compare_text(&format!("[{i}]"), text(x), text(y));
    }
    println!("\n## System note (second message)");
    let role = |m: Option<&Value>| m.and_then(|m| m.get("role")).and_then(Value::as_str).unwrap_or("-").to_owned();
    println!("  role: theirs {} | ours {}", role(a.get(1)), role(b.get(1)));
    let note = |m: Option<&Value>| blocks(m).iter().map(|b| text(b).to_owned()).collect::<Vec<_>>().join("\n");
    compare_text("note", &note(a.get(1)), &note(b.get(1)));
}

/// Byte equality, else the lines only one side has.
fn compare_text(label: &str, a: &str, b: &str) {
    if a == b {
        println!("  {label} identical, {} bytes", a.len());
        return;
    }
    println!("  {label} differs ({} vs {} bytes)", a.len(), b.len());
    let (la, lb): (Vec<String>, Vec<String>) =
        (a.lines().map(str::to_owned).collect(), b.lines().map(str::to_owned).collect());
    for line in missing(&la, &lb) {
        println!("    - {}", short(&line));
    }
    for line in missing(&lb, &la) {
        println!("    + {}", short(&line));
    }
    if la == lb {
        println!("    (same lines; differs in line endings or a trailing newline)");
    }
}

fn text(block: &Value) -> &str {
    str_of(block, "text")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Items of `a` not in `b`, in `a`'s order.
fn missing(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|x| !b.contains(x)).cloned().collect()
}

fn first_difference(a: &str, b: &str) -> Option<usize> {
    if a == b {
        return None;
    }
    let same = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    Some(boundary(a, same))
}

fn around(text: &str, at: usize) -> String {
    let start = boundary(text, at.saturating_sub(AROUND));
    let end = boundary(text, at + AROUND);
    format!("{:?}", &text[start..end])
}

fn short(text: &str) -> String {
    let end = boundary(text, WIDTH);
    if end < text.len() { format!("{}…", &text[..end]) } else { text.to_owned() }
}

/// The largest char boundary at or below `at` (clamped to the text).
fn boundary(text: &str, at: usize) -> usize {
    (0..=at.min(text.len())).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0)
}
