//! Compilation/static composition only; neither entry main/prepare/run is called.
#[allow(dead_code)]
#[path = "../src/bin/validator_authenticated_qualification.rs"]
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
    let source = include_str!("../src/bin/validator_authenticated_qualification.rs");
    assert!(source.contains("Parent::connect"));
    assert!(source.contains("into_profile_channel"));
    assert!(source.contains("run_profile_with_authenticated_qualification_channel"));
    assert!(!source.contains("termination_signals"));
    assert!(!source.contains("std::io::stdin"));
    assert!(!source.contains("process::exit("));
    assert!(include_str!("../src/bin/validator_lambda_qualification.rs").contains("run_external("));
}

fn shutdown_handle(
    dir: &std::path::Path,
    lifecycle: std::sync::Arc<light_lambda_runtime::validator_ipc::lifecycle::Lifecycle>,
) -> light_lambda::validator_deployment_profile::ProfileShutdown {
    let profile = candidate::observed_profile(
        light_lambda::validator_deployment_profile::ProfileObservation {
            lifecycle,
            drain_receipt: dir.join("drain.json"),
        },
    )
    .unwrap();
    profile
        .attach(light_lambda::input(None, false).unwrap())
        .unwrap()
        .into_parts()
        .1
}
#[tokio::test]
async fn empty_actual_drain_receipt_reports_zero_children_without_inventing_reap() {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle = light_lambda_runtime::validator_ipc::lifecycle::Lifecycle::create(
        &dir.path().join("lifecycle.jsonl"),
    )
    .unwrap();
    let handle = shutdown_handle(dir.path(), lifecycle.clone());
    handle
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
        .await
        .unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("drain.json")).unwrap()).unwrap();
    assert_eq!(receipt["observedChildren"], 0);
    assert_eq!(receipt["resourceBoundsProved"], false);
    assert!(receipt.get("childReapProved").is_none());
    assert!(lifecycle.snapshot().children.is_empty());
}
#[tokio::test]
async fn observation_receipt_never_overwrites_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drain.json");
    std::fs::write(&path, b"original synthetic receipt").unwrap();
    let lifecycle = light_lambda_runtime::validator_ipc::lifecycle::Lifecycle::create(
        &dir.path().join("lifecycle.jsonl"),
    )
    .unwrap();
    let handle = shutdown_handle(dir.path(), lifecycle);
    assert!(
        handle
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), b"original synthetic receipt");
}
