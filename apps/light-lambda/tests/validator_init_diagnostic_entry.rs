//! Compilation/static composition only; neither entry main/prepare/run is called.
#[allow(dead_code)]
#[path = "../src/bin/validator_init_diagnostic_qualification.rs"]
mod candidate;
#[allow(dead_code)]
#[path = "../src/bin/validator_lambda_qualification.rs"]
mod previous;
#[test]
fn new_entry_keeps_reviewed_source_settings_chain_limits_and_worker_identity() {
    let a = candidate::catalogue_pin();
    let b = previous::catalogue_pin();
    assert_eq!(a.source_bytes, b.source_bytes);
    assert_eq!(a.source_sha256, b.source_sha256);
    assert_eq!(a.settings, b.settings);
    assert_eq!(
        candidate::required_profile().handler_sha256,
        previous::required_profile().handler_sha256
    );
    assert_eq!(
        candidate::required_profile().active_handlers,
        previous::required_profile().active_handlers
    );
    let a = candidate::qualification_limits();
    let b = previous::qualification_limits();
    assert_eq!(a.max_source_bytes, b.max_source_bytes);
    assert_eq!(a.metadata.max_wire_bytes, b.metadata.max_wire_bytes);
    assert_eq!(a.metadata.max_peak_bytes, b.metadata.max_peak_bytes);
    assert_eq!(a.metadata.max_retained_bytes, b.metadata.max_retained_bytes);
    assert_eq!(candidate::worker_pin().bytes, 17482280);
    assert_eq!(
        candidate::worker_pin()
            .sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        "de7b4e4ea11b81283049acd70637167cab81e02d98c259ce374926fd1487b3ea"
    );
    assert_eq!(candidate::WORKER, previous::WORKER);
    assert_eq!(candidate::CONFIG, previous::CONFIG);
}
#[test]
fn observed_constructor_retains_lifecycle_without_starting_or_writing_a_drain_receipt() {
    use light_lambda_runtime::validator_ipc::lifecycle::Lifecycle;
    let dir = tempfile::tempdir().unwrap();
    let lifecycle = Lifecycle::create(&dir.path().join("owner.jsonl")).unwrap();
    let path = dir.path().join("drain.json");
    let before = std::sync::Arc::strong_count(&lifecycle);
    let profile = candidate::observed_profile(
        light_lambda::validator_deployment_profile::ProfileObservation {
            lifecycle: lifecycle.clone(),
            drain_receipt: path.clone(),
        },
    )
    .unwrap();
    assert!(std::sync::Arc::strong_count(&lifecycle) > before);
    assert!(lifecycle.snapshot().children.is_empty());
    assert!(!path.exists());
    drop(profile);
    assert!(lifecycle.snapshot().children.is_empty());
    assert!(!path.exists());
}
#[test]
fn entry_is_platform_notice_only_and_does_not_rewrite_the_old_entry() {
    let source = include_str!("../src/bin/validator_init_diagnostic_qualification.rs");
    assert!(source.contains("Parent::connect"));
    assert!(source.contains("into_profile_channel"));
    assert!(source.contains("run_profile_with_authenticated_qualification_channel"));
    assert!(!source.contains("termination_signals"));
    assert!(!source.contains("std::io::stdin"));
    assert!(!source.contains("process::exit("));
    assert!(include_str!("../src/bin/validator_lambda_qualification.rs").contains("run_external("));
}
