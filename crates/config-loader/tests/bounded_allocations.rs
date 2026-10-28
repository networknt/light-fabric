//! Isolated allocation observations; never a whole-process memory guarantee.
use config_loader::{BoundedCaptures, ConfigLoader, RuleBudget};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Observed;
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static MAX: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size(), Ordering::Relaxed);
        MAX.fetch_max(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(n, Ordering::Relaxed);
        MAX.fetch_max(n, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static GLOBAL: Observed = Observed;
fn observe(work: impl FnOnce()) -> (usize, usize, usize) {
    let calls = CALLS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    MAX.store(0, Ordering::Relaxed);
    work();
    (
        CALLS.load(Ordering::Relaxed) - calls,
        BYTES.load(Ordering::Relaxed) - bytes,
        MAX.load(Ordering::Relaxed),
    )
}
#[test]
fn bounded_parser_alias_and_borrowed_substitution_allocation_observations() {
    let directory =
        std::env::temp_dir().join(format!("wp12-bounded-allocation-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rule.yml");
    let loader = ConfigLoader::from_values(Default::default(), None, None).unwrap();
    let mut raw = b"a: small\n#".to_vec();
    raw.resize(RuleBudget::RAW, b' ');
    std::fs::write(&path, &raw).unwrap();
    let normal = observe(|| {
        let value = loader
            .load_file_bounded(&path, &mut BoundedCaptures::default())
            .unwrap();
        assert_eq!(value["a"].as_str(), Some("small"));
    });
    let raw = format!("'{}'", "x".repeat(RuleBudget::RAW - 2));
    std::fs::write(&path, &raw).unwrap();
    let scalar = observe(|| {
        assert!(
            loader
                .load_file_bounded(&path, &mut BoundedCaptures::default())
                .is_err()
        )
    });
    std::fs::write(&path,"a: &a [x,x,x,x,x,x,x,x]\nb: &b [*a,*a,*a,*a,*a,*a,*a,*a]\nc: &c [*b,*b,*b,*b,*b,*b,*b,*b]\nd: &d [*c,*c,*c,*c,*c,*c,*c,*c]\ne: [*d,*d,*d,*d,*d,*d,*d,*d]").unwrap();
    let aliases = observe(|| {
        assert!(
            loader
                .load_file_bounded(&path, &mut BoundedCaptures::default())
                .is_err()
        )
    });
    let mut sources = HashMap::new();
    sources.insert("small".into(), serde_yaml::Value::String("ok".into()));
    sources.insert(
        "huge".into(),
        serde_yaml::Value::String("x".repeat(8 * RuleBudget::RAW)),
    );
    let mut good = serde_yaml::from_str("value: ${small}").unwrap();
    let borrowed = observe(|| loader.resolve_value_bounded(&mut good, &sources).unwrap());
    let mut bad = serde_yaml::from_str("value: ${huge}").unwrap();
    let rejected = observe(|| assert!(loader.resolve_value_bounded(&mut bad, &sources).is_err()));
    assert!(borrowed.2 < RuleBudget::RAW);
    assert!(rejected.2 < RuleBudget::RAW);
    println!(
        "RULE_LOADING_ALLOCATION {}",
        serde_json::json!({"rawBytes":RuleBudget::RAW,"measurementsCallsGrossBytesLargestRequest":{"valid":normal,"oversizedScalar":scalar,"aliases":aliases,"borrowed":borrowed,"rejectedSource":rejected},"hardRssClaim":false})
    );
    // Original values parsing is a separate accounting surface. An unused
    // alias-expanded value is not governed by the effective *rule* byte limit.
    std::fs::remove_file(&path).unwrap();
    let value_source = format!(
        "seed: &s '{}'\nunused: [{}]\n",
        "x".repeat(1024),
        vec!["*s"; 256].join(",")
    );
    std::fs::write(directory.join("values.yml"), &value_source).unwrap();
    let captures =
        config_loader::BoundedLocalSources::capture(&[], None, &directory, &directory).unwrap();
    let original_values = observe(|| {
        let values = captures.values(false).unwrap();
        assert_eq!(values["unused"].as_sequence().unwrap().len(), 256);
        assert!(serde_json::to_vec(&values).unwrap().len() > RuleBudget::BYTES);
    });
    println!(
        "VALUES_SOURCE_ALLOCATION {}",
        serde_json::json!({"rawBytes":value_source.len(),"callsGrossBytesLargestRequest":original_values,"effectiveRuleBudgetAppliesToUnrelatedValues":false,"wholeProcessBound":false})
    );
    let expanded = format!(
        "seed: &s '{}'\nunused: [{}]\n",
        "x".repeat(1024),
        vec!["*s"; 1500].join(",")
    );
    std::fs::write(directory.join("values.yml"), &expanded).unwrap();
    let captures =
        config_loader::BoundedLocalSources::capture(&[], None, &directory, &directory).unwrap();
    let rejected_values = observe(|| assert!(captures.values(false).is_err()));
    // A fanout larger than the expanded document limit must be rejected by the
    // event-cost pass, before any expanded scalar/sequence is materialized.
    assert!(rejected_values.2 < RuleBudget::RAW);
    let dirs = [
        directory.join("d"),
        directory.join("l"),
        directory.join("e"),
    ];
    let aggregate_source = format!(
        "seed: &s '{}'\nunused: [{}]\n",
        "x".repeat(1024),
        vec!["*s"; 700].join(",")
    );
    for dir in &dirs {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("values.yml"), &aggregate_source).unwrap();
    }
    let captures =
        config_loader::BoundedLocalSources::capture(&[], Some(&dirs[0]), &dirs[1], &dirs[2])
            .unwrap();
    let rejected_aggregate = observe(|| assert!(captures.values(true).is_err()));
    assert!(rejected_aggregate.2 < RuleBudget::RAW);
    println!(
        "VALUES_PREMATERIALIZATION_REJECTION_ALLOCATION {}",
        serde_json::json!({"perDocumentRawBytes":expanded.len(),"perDocumentCallsGrossBytesLargestRequest":rejected_values,"aggregateMemberRawBytes":aggregate_source.len(),"aggregateCallsGrossBytesLargestRequest":rejected_aggregate,"allDocumentsCheckedBeforeFirstMaterialization":true,"wholeProcessBound":false})
    );
    std::fs::remove_dir_all(directory).unwrap();
}
