//! Local non-child profile/factory gates. No positive mount proof or worker IPC.
use light_lambda::validator_deployment_profile::{DeploymentProfile, ProfileObservation};
use light_lambda_runtime::{
    bundle_snapshot::Pin,
    required_validator_profile::RequiredValidatorProfile,
    validator_ipc::{
        Engine, ReadonlyFilesystem, ReadonlyProfile, Settings, SupervisedStaticEngine,
        accounted_profile::Limits,
        accounting::MetadataLimits,
        bundle_admission::{CatalogueEngine, CataloguePin, ReadonlyCatalogueEngine},
        lifecycle::Lifecycle,
        supervision::Policy,
    },
};
use serverless_core::HandlerId;
use std::{sync::Arc, time::Duration};

fn pin() -> Pin {
    Pin {
        path: "/opt/precision/precision-worker".into(),
        bytes: 17_482_280,
        sha256: [
            222, 123, 78, 78, 161, 27, 129, 40, 48, 73, 172, 215, 6, 55, 22, 124, 171, 129, 224,
            45, 152, 194, 89, 206, 55, 73, 38, 253, 20, 135, 179, 234,
        ],
    }
}
fn policy() -> Policy {
    Policy {
        platform_bytes: 8 << 20,
        parent_requested_bytes: 2 << 20,
        worker_requested_bytes: 2 << 20,
        headroom_bytes: 4 << 20,
        worker_address_space_bytes: 4 << 20,
        lifetime_cpu_seconds: 1,
    }
}
fn limits() -> Limits {
    Limits {
        max_source_bytes: 1024,
        metadata: MetadataLimits {
            max_wire_bytes: 1024,
            max_peak_bytes: 65536,
            max_retained_bytes: 32768,
        },
    }
}
fn catalogue() -> CataloguePin {
    CataloguePin {
        source_bytes: 1,
        source_sha256: [2; 32],
        format: gateway_policy::openapi_operation::SourceFormat::Yaml,
        settings: Settings::default(),
    }
}
fn observed() -> ProfileObservation {
    // Existing public observation API, with an owned local file and zero child
    // records. Never widen the private synthetic constructor for this test.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty-lifecycle.jsonl");
    ProfileObservation {
        lifecycle: Lifecycle::create(&path).unwrap(),
        drain_receipt: path.with_extension("drain.json"),
    }
}
fn required() -> RequiredValidatorProfile {
    RequiredValidatorProfile {
        handler_sha256: [3; 32],
        active_handlers: &[HandlerId::Specification, HandlerId::Validator],
    }
}
fn build(
    pin: Pin,
    limits: Option<Limits>,
) -> Result<DeploymentProfile, light_lambda_runtime::init_error::InitError> {
    DeploymentProfile::new_readonly_observed(
        pin,
        catalogue(),
        required(),
        policy(),
        limits,
        Duration::from_secs(1),
        observed(),
        ReadonlyProfile::pinned_layer_candidate(ReadonlyFilesystem::Ext4),
    )
}
#[test]
fn construction_and_real_factory_attachment_do_not_open_or_execute_pinned_layer() {
    let (input, shutdown) = build(pin(), Some(limits()))
        .unwrap()
        .attach(light_lambda::input(None, false).unwrap())
        .unwrap()
        .into_parts();
    assert_eq!(
        shutdown.snapshot(),
        light_lambda_runtime::validator_ipc::ownership::Drain::process().snapshot()
    );
    for id in [HandlerId::Specification, HandlerId::Validator] {
        assert!(
            input
                .handlers
                .iter()
                .find(|r| r.id == id)
                .unwrap()
                .factory
                .as_ref()
                .unwrap()
                .required_execution_profile()
                .is_some()
        );
    }
    // Do not close process-wide ownership as test cleanup or initialize worker.
}
#[test]
fn wrong_worker_root_size_and_hash_reject_before_factory_attachment() {
    let mut wrong = pin();
    wrong.path = "/opt/../precision/precision-worker".into();
    assert!(build(wrong, Some(limits())).is_err());
    let mut wrong = pin();
    wrong.bytes -= 1;
    assert!(build(wrong, Some(limits())).is_err());
    let mut wrong = pin();
    wrong.sha256[0] ^= 1;
    assert!(build(wrong, Some(limits())).is_err());
}
#[test]
fn explicit_limits_and_original_required_registration_checks_remain() {
    assert!(build(pin(), None).is_err());
    let mut l = limits();
    l.metadata.max_retained_bytes = l.metadata.max_peak_bytes + 1;
    assert!(build(pin(), Some(l)).is_err());
    let mut input = light_lambda::input(None, false).unwrap();
    input.handlers.retain(|r| r.id != HandlerId::Validator);
    assert!(build(pin(), Some(limits())).unwrap().attach(input).is_err());
}
fn engine() -> (ReadonlyCatalogueEngine, Arc<Lifecycle>) {
    let lifecycle = observed().lifecycle;
    let engine = ReadonlyCatalogueEngine::new_observed(
        Arc::new(CatalogueEngine {
            engine: Arc::new(SupervisedStaticEngine {
                executable: pin(),
                policy: policy(),
            }),
            catalogue: catalogue(),
        }),
        ReadonlyProfile::pinned_layer_candidate(ReadonlyFilesystem::Ext4),
        lifecycle.clone(),
    )
    .unwrap();
    (engine, lifecycle)
}
#[tokio::test]
async fn catalogue_mismatch_and_expired_deadline_stop_before_owned_capture() {
    use light_lambda_runtime::validator_ipc::Failure;
    let (engine, observed) = engine();
    assert!(matches!(
        engine
            .initialize(
                Arc::from(b"wrong".as_slice()),
                gateway_policy::openapi_operation::SourceFormat::Yaml,
                Settings::default(),
                tokio::time::Instant::now() + Duration::from_secs(1)
            )
            .await,
        Err(Failure::AdapterInvariant)
    ));
    assert!(matches!(
        engine
            .initialize(
                Arc::from(b"x".as_slice()),
                gateway_policy::openapi_operation::SourceFormat::Yaml,
                Settings::default(),
                tokio::time::Instant::now()
            )
            .await,
        Err(Failure::Deadline)
    ));
    let state = observed.snapshot();
    assert!(state.children.is_empty());
    assert_eq!(state.active_owners, 0);
}
#[tokio::test]
async fn offline_factory_validation_rejects_precision_metadata_without_engine_start() {
    use light_lambda_runtime::init;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let handler = json!({"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@validator",class(HandlerId::Validator))],"defaultHandlers":["spec","validator"]});
    for (name, value) in [
        ("values.yml", "light-lambda.stage: test-only\nlight-lambda.ingressTimeoutMs: 1000\nlight-lambda.responseReserveMs: 50\nlight-lambda.postReserveMs: 20\nlambda-proxy.region: us-east-1\nlambda-proxy.metricsInjection: false\n".into()),
        ("handler.yml", handler.to_string()),
        ("openapi-handler.yml", "enabled: true\n".into()),
        ("openapi-validator.yml", "enabled: true\n".into()),
        (
            "openapi.json",
            r#"{"openapi":"3.0.0","info":{"title":"synthetic","version":"1"},"paths":{}}"#.into(),
        ),
        (
            "lambda-proxy.yml",
            "functions: {'/x@post': TestFn}\nregion: us-east-1\nmetricsInjection: false\n".into(),
        ),
    ] {
        std::fs::write(dir.path().join(name), value).unwrap();
    }
    let required = RequiredValidatorProfile {
        handler_sha256:
            light_lambda_runtime::required_validator_profile::handler_identity_for_review(&handler)
                .unwrap(),
        active_handlers: &[HandlerId::Specification, HandlerId::Validator],
    };
    let obs = observed();
    let lifecycle = obs.lifecycle.clone();
    let profile = DeploymentProfile::new_readonly_observed(
        pin(),
        catalogue(),
        required,
        policy(),
        Some(limits()),
        Duration::from_secs(1),
        obs,
        ReadonlyProfile::pinned_layer_candidate(ReadonlyFilesystem::Ext4),
    )
    .unwrap();
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input
        .handlers
        .retain(|r| matches!(r.id, HandlerId::Specification | HandlerId::Validator));
    let (input, _shutdown) = profile.attach(input).unwrap().into_parts();
    let report = init::validate_configuration(&input).await;
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("offline validation does not start an engine")),
        "{:?}",
        report.errors
    );
    let state = lifecycle.snapshot();
    assert_eq!(state.active_owners, 0);
    assert!(state.children.is_empty());
}
