//! What translating to and from the Responses API costs, next to the
//! Messages request it starts from. Opt-in:
//!
//! ```bash
//! cargo test -p eos-agent-openai --release --test translation_cost -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use eos_agent_anthropic::AnthropicClient;
use eos_agent_core::{Credential, ModelRequest, StaticCredential, StoredMessage, SystemPrompt, Thinking};
use eos_agent_openai::{ClientConfig, OpenAiClient};
use eos_agent_testkit::{AllocStats, CountingAlloc};
use eos_agent_types::{ContentBlock, Role, ToolResultContent};
use serde_json::json;
use serde_json::value::RawValue;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// A conversation of `rounds` tool-using turns, four messages each (the
/// soak scenario's shape), with tool results of `result_bytes` and
/// reasoning that carries `reasoning_bytes` of encrypted content.
fn history(rounds: usize, result_bytes: usize, reasoning_bytes: usize) -> Vec<StoredMessage> {
    let mut out = Vec::with_capacity(rounds * 4);
    let text = |t: String| ContentBlock::Text { text: t, citations: None };
    for i in 0..rounds {
        out.push(
            StoredMessage::new(Role::User, &[text(format!("Round {i}: read the notes file and say what it holds."))])
                .expect("m"),
        );
        let input = RawValue::from_string(json!({"file_path": format!("/work/project/notes-{i}.txt")}).to_string())
            .expect("raw");
        out.push(
            StoredMessage::new(
                Role::Assistant,
                &[
                    ContentBlock::Thinking {
                        thinking: "The user wants the file; read it first.".into(),
                        signature: format!("oai1.rs_{i:06}.{}", "g".repeat(reasoning_bytes)),
                    },
                    ContentBlock::ToolUse { id: format!("call_{i:06}"), name: "Read".into(), input },
                ],
            )
            .expect("m"),
        );
        let result = format!("     1\t{}\n", "line of \"notes\" ".repeat(result_bytes / 16));
        out.push(
            StoredMessage::new(
                Role::User,
                &[ContentBlock::ToolResult {
                    tool_use_id: format!("call_{i:06}"),
                    content: Some(ToolResultContent::Text(result)),
                    is_error: false,
                }],
            )
            .expect("m"),
        );
        out.push(StoredMessage::new(Role::Assistant, &[text("Done.".into())]).expect("m"));
    }
    out.push(StoredMessage::user_text("And now?"));
    out
}

fn request(messages: &[StoredMessage], tools: &bytes::Bytes, system: &SystemPrompt) -> ModelRequest {
    ModelRequest {
        model: "gpt-5.5".into(),
        max_tokens: 128_000,
        system: system.json().clone(),
        tools: Some(tools.clone()),
        messages: messages.iter().map(|m| m.json().clone()).collect(),
        thinking: Some(Thinking::Adaptive { summarized: false }),
        effort: Some("medium".into()),
        extra: None,
        cache_key: Some("root".into()),
        betas: &[],
        auth: None,
    }
}

/// Mean time and allocations of `f` over `runs` calls (after one warm-up).
fn measure(runs: u32, mut f: impl FnMut() -> usize) -> (Duration, f64, usize) {
    let mut size = f();
    let before = AllocStats::now();
    let start = Instant::now();
    for _ in 0..runs {
        size = std::hint::black_box(f());
    }
    let elapsed = start.elapsed() / runs;
    let (allocs, _) = AllocStats::now().since(&before);
    (elapsed, allocs as f64 / f64::from(runs), size)
}

#[test]
#[ignore = "benchmark: run with --release --ignored --nocapture"]
fn translation_cost() {
    let cred = || Arc::new(StaticCredential(Credential::Bearer("t".into())));
    let openai = OpenAiClient::new(cred(), ClientConfig::default()).expect("client");
    let anthropic = AnthropicClient::new(cred(), eos_agent_anthropic::ClientConfig::default()).expect("client");
    // A toolbox the size of the SDK's (about 20 tools, ~45 KB).
    let tools = bytes::Bytes::from(
        serde_json::to_vec(&(0..20).map(|i| json!({"name": format!("Tool{i}"), "description": "d".repeat(2000),
            "input_schema": {"type": "object", "properties": {"x": {"type": "string", "description": "y".repeat(200)}}}}))
            .collect::<Vec<_>>())
        .expect("tools"),
    );
    let system = SystemPrompt::new(&[("You are Eos.".repeat(800).as_str(), None)]);

    println!("| History | Messages | Body | Responses body: CPU | allocations | Messages body: CPU | allocations |");
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for (label, rounds, result, reasoning) in [
        ("soak (100 rounds, short results)", 100, 48, 0),
        ("100 rounds, 1 KB reasoning each", 100, 48, 1024),
        ("1 MB (250 rounds, 3 KB results, 1 KB reasoning)", 250, 3000, 1024),
    ] {
        let messages = history(rounds, result, reasoning);
        let req = request(&messages, &tools, &system);
        let (o_time, o_allocs, size) =
            measure(200, || openai.body(&req).expect("body").iter().map(bytes::Bytes::len).sum());
        let (a_time, a_allocs, _) = measure(200, || anthropic.body(&req).iter().map(bytes::Bytes::len).sum());
        println!(
            "| {label} | {} | {:.0} KB | {:.3} ms | {o_allocs:.0} | {:.4} ms | {a_allocs:.0} |",
            messages.len(),
            size as f64 / 1024.0,
            o_time.as_secs_f64() * 1e3,
            a_time.as_secs_f64() * 1e3
        );
    }

    // The stream: a reply of 200 text deltas, reasoning and one tool call.
    let mut events = vec![json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}).to_string()];
    events.push(
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}})
            .to_string(),
    );
    events.push(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"g".repeat(1024)}}).to_string());
    events.push(
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"m1"}}).to_string(),
    );
    for _ in 0..200 {
        events.push(
            json!({"type":"response.output_text.delta","item_id":"m1","output_index":1,"delta":"word "}).to_string(),
        );
    }
    events.push(
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"m1","content":[]}})
            .to_string(),
    );
    events.push(json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"f1","call_id":"c1","name":"Read"}}).to_string());
    events.push(
        json!({"type":"response.function_call_arguments.delta","item_id":"f1","delta":"{\"file_path\":\"/a\"}"})
            .to_string(),
    );
    events.push(json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"f1","call_id":"c1","name":"Read","arguments":"{\"file_path\":\"/a\"}"}}).to_string());
    events.push(
        json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}}).to_string(),
    );
    let (time, allocs, out) =
        measure(2000, || eos_agent_openai::translate_stream(events.iter().map(String::as_str)).expect("events").len());
    let per = events.len() as f64;
    println!(
        "\nStream: {} Responses events -> {out} Messages events: {:.2} µs and {:.1} allocations per event",
        events.len(),
        time.as_secs_f64() * 1e6 / per,
        allocs / per
    );
    // The same reply as Messages events, parsed as the Anthropic client parses them.
    let delta = r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"word "}}"#;
    let messages: Vec<(&str, &str)> = std::iter::repeat_n(("content_block_delta", delta), 200).collect();
    let (time, allocs, _) = measure(2000, || {
        messages.iter().filter_map(|(name, data)| eos_agent_types::StreamEvent::parse(name, data).ok()).count()
    });
    println!(
        "Messages stream (200 text deltas): {:.2} µs and {:.1} allocations per event",
        time.as_secs_f64() * 1e6 / 200.0,
        allocs / 200.0
    );
}
