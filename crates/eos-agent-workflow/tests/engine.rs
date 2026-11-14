//! The QuickJS realm on its own: host calls out, answers in, the sandbox.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eos_agent_workflow::engine::{Fatal, Host, HostError, QuickJs, RealmSetup, SLICE_LIMIT, ScriptEngine};

/// Records every host call; `agent` and `timer` answer with increasing ids.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, String)>>,
    next: Mutex<u64>,
}

impl Recorder {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("lock").clone()
    }

    fn last(&self, op: &str) -> Option<String> {
        self.calls().into_iter().rev().find(|(o, _)| o == op).map(|(_, p)| p)
    }
}

impl Host for Recorder {
    fn call(&self, _realm: u32, op: &str, payload: &str) -> Result<String, HostError> {
        self.calls.lock().expect("lock").push((op.to_owned(), payload.to_owned()));
        match op {
            "agent" | "timer" | "workflow" => {
                let mut next = self.next.lock().expect("lock");
                *next += 1;
                Ok(next.to_string())
            }
            "spent" => Ok("1234".into()),
            "fork" => Ok("0".into()),
            "slots" => {
                let n = serde_json::from_str::<serde_json::Value>(payload).expect("json")["n"].as_u64().unwrap_or(0);
                Ok(serde_json::to_string(&vec![0; n as usize]).expect("json"))
            }
            "caps" => Ok("null".into()),
            _ => Ok("null".into()),
        }
    }
}

fn setup(body: &str) -> RealmSetup {
    RealmSetup { id: 0, body: body.into(), config: r#"{"total":null,"args":"[1,2]"}"#.into(), slice_limit: SLICE_LIMIT }
}

#[test]
fn agent_calls_resolve_from_the_host() {
    let host = Arc::new(Recorder::default());
    let body = "const a = await agent('one', {label: 'L'}); const b = await agent(`two ${a}`); return {a, b, args}";
    let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
    realm.start().expect("start");
    let first: serde_json::Value = serde_json::from_str(&host.last("agent").expect("agent call")).expect("json");
    assert_eq!(first["prompt"], "one");
    assert_eq!(first["opts"]["label"], "L");
    realm.settle(1, true, "\"A\"").expect("settle");
    let second: serde_json::Value = serde_json::from_str(&host.last("agent").expect("second call")).expect("json");
    assert_eq!(second["prompt"], "two A");
    assert!(host.last("done").is_none());
    realm.settle(2, true, "{\"x\":1}").expect("settle");
    let done: serde_json::Value = serde_json::from_str(&host.last("done").expect("done")).expect("json");
    assert_eq!(done, serde_json::json!({"a": "A", "b": {"x": 1}, "args": [1, 2]}));
}

#[test]
fn rejections_and_parallel_slots() {
    let host = Arc::new(Recorder::default());
    let body = "return await parallel([() => agent('a'), () => agent('b'), () => { throw new Error('boom') }])";
    let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
    realm.start().expect("start");
    realm.settle(1, true, "\"ok\"").expect("settle");
    realm.settle(2, false, r#"{"name":"Error","message":"api down"}"#).expect("settle");
    let done = host.last("done").expect("done");
    assert_eq!(done, r#"["ok",null,null]"#);
    let failures: Vec<String> = host.calls().into_iter().filter(|(o, _)| o == "failure").map(|(_, p)| p).collect();
    assert_eq!(failures.len(), 2);
    assert!(failures[0].contains("parallel[1] failed: api down"), "{failures:?}");
    assert!(failures[1].contains("parallel[2] failed: boom"), "{failures:?}");
}

#[test]
fn determinism_shims_and_no_code_from_strings() {
    let cases = [
        ("return Date.now()", "Date.now() / new Date() are unavailable"),
        ("return new Date()", "Date.now() / new Date() are unavailable"),
        ("return Date()", "Date.now() / new Date() are unavailable"),
        ("return Math.random()", "Math.random() is unavailable"),
        ("return eval('1')", "eval is not defined"),
        ("return Function('return 1')()", "Code generation from strings disallowed"),
        ("return (async () => {}).constructor('return 1')", "Code generation from strings disallowed"),
        ("return new Date(0).constructor.now()", "Date.now() / new Date() are unavailable"),
        ("return typeof require + typeof process + typeof fetch", ""),
    ];
    for (body, expected) in cases {
        let host = Arc::new(Recorder::default());
        let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
        realm.start().expect("start");
        if expected.is_empty() {
            assert_eq!(host.last("done").as_deref(), Some("\"undefinedundefinedundefined\""), "{body}");
        } else {
            let fail = host.last("fail").unwrap_or_else(|| panic!("{body} should fail"));
            assert!(fail.contains(expected), "{body}: {fail}");
        }
    }
    let host = Arc::new(Recorder::default());
    let mut realm = QuickJs.realm(setup("return new Date(86400000).toISOString()"), host.clone()).expect("realm");
    realm.start().expect("start");
    assert_eq!(host.last("done").as_deref(), Some("\"1970-01-02T00:00:00.000Z\""));
}

#[test]
fn a_busy_loop_is_cut_at_the_slice_limit() {
    let host = Arc::new(Recorder::default());
    let mut s = setup("await agent('x'); while (true) {}");
    s.slice_limit = Duration::from_millis(50);
    let mut realm = QuickJs.realm(s, host.clone()).expect("realm");
    realm.start().expect("the first stretch only queues an agent");
    let started = Instant::now();
    // The loop runs after the agent resolves: every stretch is bounded.
    let err = realm.settle(1, true, "null").expect_err("timed out");
    assert_eq!(err, Fatal::TimedOut(Duration::from_millis(50)));
    assert_eq!(err.to_string(), "Script execution timed out after 50ms");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn timers_fire_from_the_host() {
    let host = Arc::new(Recorder::default());
    let body = "await new Promise(r => setTimeout(r, 10)); return budget.spent() + (budget.total === null ? 1 : 0)";
    let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
    realm.start().expect("start");
    assert_eq!(host.last("timer").as_deref(), Some(r#"{"ms":10}"#));
    realm.fire_timer(1).expect("fire");
    assert_eq!(host.last("done").as_deref(), Some("1235"));
}

#[test]
fn syntax_errors_name_the_line() {
    let err = QuickJs.check_syntax("const a = 1\nconst b = ;").expect_err("syntax error");
    assert_eq!(err.line, Some(2), "{err:?}");
    assert!(err.to_string().starts_with("SyntaxError: "), "{err}");
    QuickJs.check_syntax("const x = await agent('a'); return x").expect("compiles");
    QuickJs.check_syntax("while (true) {}").expect("compiling does not run");
}

/// The rewrite for lanes never moves a position: runtime errors and the
/// early errors QuickJS finds point where the script has them.
#[test]
fn positions_are_the_scripts_own() {
    let location = |body: &str| {
        let host = Arc::new(Recorder::default());
        let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
        realm.start().expect("start");
        let fail: serde_json::Value = serde_json::from_str(&host.last("fail").expect("fail")).expect("json");
        let stack = fail["stack"].as_str().expect("stack").to_owned();
        let at = stack.find("workflow.js:").expect("frame") + "workflow.js:".len();
        stack[at..].split(')').next().expect("position").to_owned()
    };
    // Same columns, one line with awaits (rewritten), one without.
    let rewritten = location("const a = 1\nawait 0; await 0; throw new Error('b')");
    assert_eq!(rewritten, location("const a = 1\nvoid  0; void  0; throw new Error('b')"));
    assert!(rewritten.starts_with("3:"), "{rewritten}");

    let early = QuickJs.check_syntax("await 0; let x = 1; let x = 2").expect_err("redeclared");
    let plain = QuickJs.check_syntax("void  0; let x = 1; let x = 2").expect_err("redeclared");
    assert_eq!((early.line, early.column), (plain.line, plain.column));
    assert_eq!(early.message, plain.message);
    // oxc reads it first: its message, the column in characters.
    let err = QuickJs.check_syntax("const é = 1\nconst x: string = 1").expect_err("not JavaScript");
    assert_eq!((err.line, err.column), (Some(2), Some(7)));
}

#[test]
fn values_crossing_the_boundary_are_capped() {
    let host = Arc::new(Recorder::default());
    let body = "return await agent('a', {schema: {big: new Array(5000).fill(0)}})";
    let mut realm = QuickJs.realm(setup(body), host.clone()).expect("realm");
    realm.start().expect("start");
    let fail = host.last("fail").expect("refused");
    assert!(fail.contains("array length 5000 exceeds the maximum of 4096"), "{fail}");
    let host = Arc::new(Recorder::default());
    let mut realm = QuickJs.realm(setup("return () => 1"), host.clone()).expect("realm");
    realm.start().expect("start");
    assert!(host.last("fail").expect("fail").contains("workflow result cannot be a function"));
}

/// Claude Code's regex-or-division check passes every `/` QuickJS reads as
/// oxc's parser does, however the statements around it are laid out.
#[test]
fn slashes_read_one_way_pass_the_regex_or_division_check() {
    let bodies = [
        "const a = 6 / 2 / 3\nconst b = /x\\/y/g.test('x/y') ? a /= 2 : a\nreturn [a, b]",
        "let n = 4\nn\n/2/1\nconst r = `${n / 2}` + /[/]/.source\nreturn r",
        "if (true) /re/.exec('re')\nfunction f() {}\n/re/.test('re')\nconst o = {} / 1\nreturn o",
        "const x = (await agent('a')).length / 2 // a comment / with slashes\n/* and / here */\nreturn x",
        "for await (const v of [1]) { const q = v / 2; }\nreturn [1, 2].map(i => i / 2).filter(i => /1/.test(String(i)))",
    ];
    for body in bodies {
        assert!(QuickJs.check_syntax(body).is_ok(), "{body}");
        assert_eq!(QuickJs.ambiguous_slash(body), None, "{body}");
    }
}
