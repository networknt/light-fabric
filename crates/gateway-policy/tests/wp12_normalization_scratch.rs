//! Test-binary-only allocation observation, not a production allocator or RSS bound.
use gateway_policy::openapi_operation::BasePathProfile;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    time::Instant,
};
thread_local! {
    static MEASURE:Cell<bool>=const{Cell::new(false)};
    static LIVE:Cell<usize>=const{Cell::new(0)};
    static PEAK:Cell<usize>=const{Cell::new(0)};
}
struct Allocator;
fn add(bytes: usize) {
    MEASURE.with(|flag| {
        if flag.get() {
            LIVE.with(|live| {
                let n = live.get() + bytes;
                live.set(n);
                PEAK.with(|peak| peak.set(peak.get().max(n)));
            });
        }
    });
}
fn remove(bytes: usize) {
    MEASURE.with(|flag| {
        if flag.get() {
            LIVE.with(|live| live.set(live.get().saturating_sub(bytes)));
        }
    });
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        remove(layout.size());
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(p, layout, size) };
        if !out.is_null() {
            remove(layout.size());
            add(size);
        }
        out
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn measure(profile: &BasePathProfile, input: &str) -> (usize, usize, u128, bool, usize) {
    LIVE.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    let start = Instant::now();
    MEASURE.with(|c| c.set(true));
    let result = profile.normalize_bounded_path(input);
    let elapsed = start.elapsed().as_micros();
    MEASURE.with(|c| c.set(false));
    (
        PEAK.with(Cell::get),
        LIVE.with(Cell::get),
        elapsed,
        result.is_ok(),
        result.as_ref().map(|s| s.capacity()).unwrap_or(0),
    )
}
#[test]
fn fresh_worker_cache_scratch_and_adversarial_bounded_inputs() {
    let input = "/".to_string() + &"a".repeat(8190) + "!";
    for (id, pattern) in [
        ("nested-overlap", "/((a|aa)+)+z"),
        ("nested-counts", "/((a{64}){64}){7}"),
        ("greedy-failure", "/a+z"),
        ("lazy-failure", "/a+?z"),
        ("unicode-class", r"/[^x]*z"),
        ("intersection", "/[a-z&&[^aeiou]]+z"),
        ("near-program-alternative", "/((a{64}){64}){7}|/a*"),
        ("nullable-count-overlap", "/((a|aa){0,64}){0,64}z"),
    ] {
        let profile = BasePathProfile::from_pattern(pattern).unwrap();
        for iteration in 0..3 {
            let (peak, live, micros, ok, capacity) = measure(&profile, &input);
            assert!(ok);
            // All search scratch is dropped on return; only the output escapes.
            assert_eq!(live, capacity, "{id}: cache/scratch retained beyond search");
            println!(
                "pattern={id} iteration={iteration} input_bytes={} peak_allocated_bytes={peak} live_output_bytes={live} elapsed_us={micros}",
                input.len()
            );
        }
    }
    let profile = BasePathProfile::from_pattern("/api").unwrap();
    let oversized = "x".repeat(8193);
    let (peak, live, _, ok, _) = measure(&profile, &oversized);
    assert!(!ok);
    assert_eq!(
        (peak, live),
        (0, 0),
        "neutral oversize entry must allocate nothing"
    );
}
