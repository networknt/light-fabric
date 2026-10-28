//! Separate unexecuted diagnostic candidate; original authenticated entry intact.
use light_lambda::{
    validator_deployment_profile::{DeploymentProfile, ProfileObservation},
    validator_deployment_runtime::run_profile_with_authenticated_qualification_channel,
};
use light_lambda_runtime::{
    init_error::InitError,
    qualification_extension_bridge::{self as bridge, Parent},
    qualification_shutdown::{CLEANUP_CAP, DEADLINE_RESERVE, INIT_TIMEOUT},
    validator_ipc::{
        accounted_profile::Limits, accounting::MetadataLimits, lifecycle::Lifecycle,
        supervision::Policy,
    },
};
use std::sync::Arc;
use std::{
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
pub const CONFIG: &str = "/opt/config";
pub const WORKER: &str = "/opt/precision/precision-worker";
const STARTUP_ERROR: &str = "validator: authenticated qualification startup unavailable";
fn invalid() -> InitError {
    InitError::InvalidConfiguration(STARTUP_ERROR)
}
fn main() -> ExitCode {
    light_lambda_runtime::telemetry::install_panic_redaction();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("validator: authenticated qualification startup unavailable");
            return ExitCode::FAILURE;
        }
    };
    let success = runtime.block_on(run());
    runtime.shutdown_timeout(DEADLINE_RESERVE);
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
async fn run() -> bool {
    let diagnostic =
        Arc::new(light_lambda_runtime::validator_init_diagnostic::InitDiagnostic::default());
    let observed = diagnostic.clone();
    let result = async {
        bridge::validate_policy(&policy()).map_err(|_| invalid())?;
        let deadline = Instant::now()
            .checked_add(INIT_TIMEOUT)
            .ok_or_else(invalid)?;
        let digest = std::env::var(bridge::MANIFEST_ENV).map_err(|_| invalid())?;
        let manifest =
            bridge::read_manifest(std::path::Path::new(bridge::MANIFEST), &digest, deadline)
                .map_err(|_| invalid())?;
        let channel = Parent::connect(&manifest, deadline)
            .await
            .map_err(|_| invalid())?
            .into_profile_channel()
            .map_err(|_| invalid())?;
        run_profile_with_authenticated_qualification_channel(
            move || prepare(observed),
            Some(light_lambda::phase1_guard()),
            channel,
            CLEANUP_CAP,
        )
        .await
    }
    .await;
    diagnostic.emit_after_run();
    match result {
        Ok(receipt) => receipt.completed_without_failure(),
        Err(error) => {
            let authority = std::env::var("AWS_LAMBDA_RUNTIME_API").ok();
            let _reported =
                light_lambda_runtime::init_error::report(&error, authority.as_deref()).await;
            eprintln!("validator: authenticated qualification startup unavailable");
            false
        }
    }
}
pub fn worker_pin() -> light_lambda_runtime::bundle_snapshot::Pin {
    light_lambda_runtime::bundle_snapshot::Pin {
        path: WORKER.into(),
        bytes: 17482280,
        sha256: [
            222, 123, 78, 78, 161, 27, 129, 40, 48, 73, 172, 215, 6, 55, 22, 124, 171, 129, 224,
            45, 152, 194, 89, 206, 55, 73, 38, 253, 20, 135, 179, 234,
        ],
    }
}
pub fn observed_profile(observation: ProfileObservation) -> Result<DeploymentProfile, InitError> {
    DeploymentProfile::new_observed(
        worker_pin(),
        catalogue_pin(),
        Arc::try_unwrap(required_profile()).map_err(|_| invalid())?,
        policy(),
        Some(qualification_limits()),
        INIT_TIMEOUT,
        observation,
    )
}
fn prepare(
    diagnostic: Arc<light_lambda_runtime::validator_init_diagnostic::InitDiagnostic>,
) -> Result<light_lambda::validator_deployment_profile::ConfiguredProfile, InitError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid())?
        .as_nanos();
    let path = PathBuf::from(format!(
        "/tmp/validator-qualification-{}-{stamp}.jsonl",
        std::process::id()
    ));
    let lifecycle = Lifecycle::create(&path).map_err(|_| invalid())?;
    let profile = observed_profile(ProfileObservation {
        lifecycle,
        drain_receipt: path.with_extension("drain.json"),
    })?;
    profile
        .with_init_diagnostic(diagnostic)
        .attach(light_lambda::input(Some(PathBuf::from(CONFIG)), true)?)
}
fn policy() -> Policy {
    // Existing experimental named-profile plan; not proven memory bounds/defaults.
    Policy {
        parent_requested_bytes: 268435456,
        worker_requested_bytes: 536870912,
        worker_address_space_bytes: 1073741824,
        lifetime_cpu_seconds: 16,
        platform_bytes: 1073741824,
        headroom_bytes: 268435456,
    }
}

pub fn qualification_limits() -> Limits {
    Limits {
        max_source_bytes: 17065,
        metadata: MetadataLimits {
            max_wire_bytes: 262144,
            max_peak_bytes: 33554432,
            max_retained_bytes: 16777216,
        },
    }
}

pub fn catalogue_pin() -> light_lambda_runtime::validator_ipc::bundle_admission::CataloguePin {
    light_lambda_runtime::validator_ipc::bundle_admission::CataloguePin {
        source_bytes: 17065,
        source_sha256: [
            1, 46, 253, 243, 104, 45, 137, 63, 124, 71, 55, 56, 209, 88, 97, 95, 47, 10, 41, 61,
            152, 135, 101, 239, 70, 118, 235, 51, 7, 196, 97, 1,
        ],
        format: light_lambda_runtime::qualification_shutdown::SourceFormat::Yaml,
        settings: light_lambda_runtime::validator_ipc::Settings {
            skip_body_validation: Some(false),
            handle_nullable_field: Some(true),
            skip_path_prefixes: Some(vec![]),
        },
    }
}

pub fn required_profile()
-> Arc<light_lambda_runtime::required_validator_profile::RequiredValidatorProfile> {
    use serverless_core::HandlerId;
    Arc::new(
        light_lambda_runtime::required_validator_profile::RequiredValidatorProfile {
            handler_sha256: [
                131, 15, 156, 58, 154, 87, 14, 59, 76, 178, 70, 161, 95, 41, 26, 2, 252, 214, 77,
                30, 244, 184, 176, 253, 163, 190, 5, 54, 201, 20, 252, 183,
            ],
            active_handlers: &[
                HandlerId::Metrics,
                HandlerId::RequestCors,
                HandlerId::ResponseCors,
                HandlerId::Specification,
                HandlerId::Validator,
            ],
        },
    )
}
