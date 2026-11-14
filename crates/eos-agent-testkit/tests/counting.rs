use eos_agent_testkit::{AllocStats, CountingAlloc};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

#[test]
fn counts_allocations_and_returns_to_baseline() {
    let before = AllocStats::now();
    let v: Vec<u8> = Vec::with_capacity(4096);
    let mid = AllocStats::now();
    assert!(mid.since(&before).0 >= 1);
    assert!(mid.since(&before).1 >= 4096);
    drop(v);
    let after = AllocStats::now();
    assert!(after.since(&before).1 <= 0, "freed memory still counted as live");
}
