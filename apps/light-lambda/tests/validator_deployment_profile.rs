//! Non-child constructor/composition tests; never initialize the engine.
use light_lambda::validator_deployment_profile::DeploymentProfile;
use light_lambda_runtime::{
    bundle_snapshot::Pin,
    required_validator_profile::RequiredValidatorProfile,
    validator_ipc::{
        Settings, accounted_profile::Limits, accounting::MetadataLimits,
        bundle_admission::CataloguePin, supervision::Policy,
    },
};
use serverless_core::HandlerId;
use std::{path::PathBuf, time::Duration};

struct Candidate {
    worker: Pin,
    catalogue: CataloguePin,
    required: RequiredValidatorProfile,
    policy: Policy,
    limits: Option<Limits>,
    timeout: Duration,
}
impl Candidate {
    fn build(self) -> Result<DeploymentProfile, light_lambda_runtime::init_error::InitError> {
        DeploymentProfile::new(
            self.worker,
            self.catalogue,
            self.required,
            self.policy,
            self.limits,
            self.timeout,
        )
    }
}
fn candidate() -> Candidate {
    // Synthetic construction-only values. No physical bound/default claim.
    Candidate {
        worker: Pin {
            path: "/does-not-exist/deployment-profile-worker".into(),
            bytes: 64,
            sha256: [1; 32],
        },
        catalogue: CataloguePin {
            source_bytes: 1,
            source_sha256: [2; 32],
            format: gateway_policy::openapi_operation::SourceFormat::Yaml,
            settings: Settings {
                skip_body_validation: Some(false),
                handle_nullable_field: Some(true),
                skip_path_prefixes: Some(vec![]),
            },
        },
        required: RequiredValidatorProfile {
            handler_sha256: [3; 32],
            active_handlers: &[HandlerId::Specification, HandlerId::Validator],
        },
        policy: Policy {
            platform_bytes: 1048576,
            parent_requested_bytes: 262144,
            worker_requested_bytes: 262144,
            headroom_bytes: 524288,
            worker_address_space_bytes: 1048576,
            lifetime_cpu_seconds: 1,
        },
        limits: Some(Limits {
            max_source_bytes: 1,
            metadata: MetadataLimits {
                max_wire_bytes: 1024,
                max_peak_bytes: 65536,
                max_retained_bytes: 32768,
            },
        }),
        timeout: Duration::from_secs(1),
    }
}
#[test]
fn construction_does_not_open_the_nonexistent_worker() {
    assert!(candidate().build().is_ok());
}
#[test]
fn missing_limits_reject() {
    let mut c = candidate();
    c.limits = None;
    assert!(c.build().is_err());
}
#[test]
fn zero_and_overflow_policy_reject() {
    let mut c = candidate();
    c.policy.lifetime_cpu_seconds = 0;
    assert!(c.build().is_err());
    let mut c = candidate();
    c.policy.parent_requested_bytes = u64::MAX;
    assert!(c.build().is_err());
}
#[test]
fn inconsistent_metadata_limits_reject() {
    let mut c = candidate();
    c.limits.as_mut().unwrap().metadata.max_retained_bytes = 65537;
    assert!(c.build().is_err());
}
#[test]
fn relative_or_parent_worker_paths_reject() {
    for path in ["relative-worker", "/opt/../worker"] {
        let mut c = candidate();
        c.worker.path = PathBuf::from(path);
        assert!(c.build().is_err());
    }
}
#[test]
fn empty_worker_or_absent_worker_identity_reject() {
    let mut c = candidate();
    c.worker.bytes = 0;
    assert!(c.build().is_err());
    let mut c = candidate();
    c.worker.sha256 = [0; 32];
    assert!(c.build().is_err());
}
#[test]
fn empty_source_or_absent_source_identity_reject() {
    let mut c = candidate();
    c.catalogue.source_bytes = 0;
    assert!(c.build().is_err());
    let mut c = candidate();
    c.catalogue.source_sha256 = [0; 32];
    assert!(c.build().is_err());
}
#[test]
fn source_pin_cannot_exceed_accounted_ceiling() {
    let mut c = candidate();
    c.catalogue.source_bytes = 2;
    assert!(c.build().is_err());
}
#[test]
fn absent_chain_identity_or_required_handlers_reject() {
    let mut c = candidate();
    c.required.handler_sha256 = [0; 32];
    assert!(c.build().is_err());
    let mut c = candidate();
    c.required.active_handlers = &[HandlerId::Specification];
    assert!(c.build().is_err());
}
#[test]
fn duplicate_or_proxy_active_registration_identity_reject() {
    for ids in [
        &[
            HandlerId::Specification,
            HandlerId::Validator,
            HandlerId::Validator,
        ][..],
        &[
            HandlerId::Specification,
            HandlerId::Validator,
            HandlerId::Proxy,
        ][..],
    ] {
        let mut c = candidate();
        c.required.active_handlers = ids;
        assert!(c.build().is_err());
    }
}
#[test]
fn zero_or_unrepresentable_timeout_reject() {
    let mut c = candidate();
    c.timeout = Duration::ZERO;
    assert!(c.build().is_err());
    let mut c = candidate();
    c.timeout = Duration::MAX;
    assert!(c.build().is_err());
}
#[test]
fn attachment_returns_real_existing_process_shutdown_handle() {
    let input = light_lambda::input(None, false).unwrap();
    let expected = light_lambda_runtime::validator_ipc::ownership::Drain::process().snapshot();
    let (input, shutdown) = candidate()
        .build()
        .unwrap()
        .attach(input)
        .unwrap()
        .into_parts();
    assert_eq!(shutdown.snapshot(), expected);
    for id in [HandlerId::Specification, HandlerId::Validator] {
        let provider = input
            .handlers
            .iter()
            .find(|r| r.id == id)
            .unwrap()
            .factory
            .as_ref()
            .unwrap();
        assert!(provider.required_execution_profile().is_some());
    }
    // No initialize/shutdown: never close process-wide state as test cleanup.
}
#[test]
fn missing_or_duplicate_registration_refuses_attachment() {
    let mut input = light_lambda::input(None, false).unwrap();
    input.handlers.retain(|r| r.id != HandlerId::Validator);
    assert!(candidate().build().unwrap().attach(input).is_err());
    let mut input = light_lambda::input(None, false).unwrap();
    input
        .handlers
        .push(light_lambda_runtime::init::HandlerRegistration {
            id: HandlerId::Validator,
            config_file: "openapi-validator.yml",
            factory: None,
        });
    assert!(candidate().build().unwrap().attach(input).is_err());
}
