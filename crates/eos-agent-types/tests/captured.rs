//! Every event of every captured real API response must parse, and nothing
//! the API sent may land in an `Unknown` bucket unnoticed. Captures are local
//! only (`fixtures/captured/`, see the capture tool); without them this passes.

use std::path::PathBuf;

use eos_agent_types::{Delta, StreamEvent};

fn captures() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/captured");
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<_> =
        entries.filter_map(|e| Some(e.ok()?.path().join("response.sse"))).filter(|p| p.is_file()).collect();
    files.sort();
    files
}

fn frames(body: &str) -> impl Iterator<Item = (&str, &str)> {
    body.split("\n\n").filter_map(|frame| {
        let event = frame.lines().find_map(|l| l.strip_prefix("event:"))?.trim();
        let data = frame.lines().find_map(|l| l.strip_prefix("data:"))?.trim();
        Some((event, data))
    })
}

#[test]
fn captured_streams_parse_without_unknowns() {
    for file in captures() {
        let body = std::fs::read_to_string(&file).expect("read capture");
        if !body.starts_with("event:") {
            continue; // not a stream (an error body or a JSON endpoint)
        }
        let mut stopped = false;
        for (event, data) in frames(&body) {
            let parsed = StreamEvent::parse(event, data).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            match &parsed {
                StreamEvent::Unknown { name } => panic!("{}: unknown event `{name}`", file.display()),
                StreamEvent::ContentBlockDelta { delta: Delta::Unknown(raw), .. } => {
                    panic!("{}: unknown delta {}", file.display(), raw.get())
                }
                StreamEvent::MessageStop => stopped = true,
                _ => {}
            }
        }
        assert!(stopped, "{}: stream never stopped", file.display());
    }
}
