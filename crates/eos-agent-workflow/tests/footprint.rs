//! What a workflow realm costs: memory held per open realm and the time to
//! open one. `cargo test --release -p eos-agent-workflow --test footprint --
//! --ignored --nocapture` prints the numbers.

use std::sync::Arc;
use std::time::Instant;

use eos_agent_workflow::engine::{Host, HostError, QuickJs, RealmSetup, SLICE_LIMIT, ScriptEngine};

struct Idle;

impl Host for Idle {
    fn call(&self, _realm: u32, op: &str, _payload: &str) -> Result<String, HostError> {
        Ok(if op == "agent" { "1".into() } else { "null".into() })
    }
}

fn setup() -> RealmSetup {
    // A run waiting on its first agent, as most of a run's life is spent.
    let body = "const r = await parallel([1, 2, 3].map(i => () => agent(`task ${i}`, {label: `t${i}`}))); return r";
    RealmSetup { id: 0, body: body.into(), config: r#"{"total":null}"#.into(), slice_limit: SLICE_LIMIT }
}

#[test]
#[ignore = "measurement; run with --release --ignored --nocapture"]
fn realm_footprint() {
    const N: usize = 200;
    let host: Arc<dyn Host> = Arc::new(Idle);
    // Warm up allocator pools and code pages.
    drop(QuickJs.realm(setup(), Arc::clone(&host)).expect("realm"));

    let before = eos_agent_testkit::footprint_bytes();
    let started = Instant::now();
    let mut realms = Vec::with_capacity(N);
    for _ in 0..N {
        let mut realm = QuickJs.realm(setup(), Arc::clone(&host)).expect("realm");
        realm.start().expect("start");
        realms.push(realm);
    }
    let open = started.elapsed();
    let after = eos_agent_testkit::footprint_bytes();
    let engine_bytes: usize = realms.iter().map(|r| r.memory_bytes()).sum();
    drop(realms);

    let mut opens = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        let realm = QuickJs.realm(setup(), Arc::clone(&host)).expect("realm");
        opens.push(t.elapsed());
        drop(realm);
    }
    opens.sort();
    let check = Instant::now();
    QuickJs.check_syntax(&setup().body).expect("syntax");
    let check = check.elapsed();

    println!("realms open: {N}, in {open:?} ({:?} each, started)", open / N as u32);
    println!("open alone: median {:?}, p95 {:?}", opens[N / 2], opens[N * 95 / 100]);
    println!("syntax check: {check:?}");
    println!("engine heap per realm: {:.1} KB", engine_bytes as f64 / N as f64 / 1024.0);
    println!("process footprint per realm: {:.1} KB", (after.saturating_sub(before)) as f64 / N as f64 / 1024.0);
}
