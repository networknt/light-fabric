//! Opt-in no-stdin Lambda qualification entry. Never the accepted/default bootstrap.
use light_lambda_runtime::{
    init_error::InitError,
    owned_runtime::ExitKind,
    qualification_extension_bridge::run_external,
    qualification_shutdown::{DEADLINE_RESERVE, INIT_TIMEOUT, termination_signals},
    validator_ipc::{
        Failure, SupervisedStaticEngine,
        accounted_profile::{AccountedCatalogueEngine, Limits},
        accounting::MetadataLimits,
        lifecycle::{CatalogueObservedProfileEngine, CompletionCause, Lifecycle},
        supervision::Policy,
    },
};
use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

pub const CONFIG: &str = "/opt/config";
pub const WORKER: &str = "/opt/precision/precision-worker";
const STARTUP_ERROR: &str = "validator: qualification startup unavailable";

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
            eprintln!("validator: qualification startup unavailable");
            return ExitCode::FAILURE;
        }
    };
    let success = runtime.block_on(qualification_entry());
    // A failed drain is already nonzero; this does not claim stopped blocking work.
    runtime.shutdown_timeout(DEADLINE_RESERVE);
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
fn setup() -> Result<(Arc<AccountedCatalogueEngine>, Arc<Lifecycle>, PathBuf), InitError> {
    policy()
        .validate()
        .map_err(|_| InitError::InvalidConfiguration(STARTUP_ERROR))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| InitError::InvalidConfiguration(STARTUP_ERROR))?
        .as_nanos();
    let path = PathBuf::from(format!(
        "/tmp/validator-qualification-{}-{stamp}.jsonl",
        std::process::id()
    ));
    let lifecycle =
        Lifecycle::create(&path).map_err(|_| InitError::InvalidConfiguration(STARTUP_ERROR))?;
    let engine = profile_engine(lifecycle.clone(), Some(qualification_limits()))
        .map_err(|_| InitError::InvalidConfiguration(STARTUP_ERROR))?;
    Ok((engine, lifecycle, path))
}

/// Explicit test-only candidate ceilings, not production defaults or memory bounds.
/// Limits tighten existing wire ceilings and do not reserve their full maxima.
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

/// This helper constructs only the pinned trusted observed catalogue engine.
/// It never calls initialize, starts a child or installs a default selector.
pub fn profile_engine(
    lifecycle: Arc<Lifecycle>,
    limits: Option<Limits>,
) -> Result<Arc<AccountedCatalogueEngine>, Failure> {
    let engine = Arc::new(CatalogueObservedProfileEngine {
        engine: Arc::new(
            light_lambda_runtime::validator_ipc::bundle_admission::CatalogueEngine {
                catalogue: catalogue_pin(),
                engine: Arc::new(SupervisedStaticEngine {
                    executable: light_lambda_runtime::bundle_snapshot::Pin {
                        path: WORKER.into(),
                        bytes: 17482280,
                        sha256: [
                            222, 123, 78, 78, 161, 27, 129, 40, 48, 73, 172, 215, 6, 55, 22, 124,
                            171, 129, 224, 45, 152, 194, 89, 206, 55, 73, 38, 253, 20, 135, 179,
                            234,
                        ],
                    },
                    policy: policy(),
                }),
            },
        ),
        lifecycle,
    });
    AccountedCatalogueEngine::new_observed(engine, limits).map(Arc::new)
}

async fn qualification_entry() -> bool {
    let observation = Arc::new(Mutex::new(None));
    let configured = observation.clone();
    let outcome = run_external(
        policy(),
        termination_signals(),
        move || {
            let (engine, lifecycle, path) = setup()?;
            *configured
                .lock()
                .map_err(|_| InitError::InvalidConfiguration(STARTUP_ERROR))? =
                Some((lifecycle, path));
            trusted_input(Some(PathBuf::from(CONFIG)), true, engine, INIT_TIMEOUT)
        },
        Some(light_lambda::phase1_guard()),
        move |kind, deadline| async move {
            let observed = observation
                .lock()
                .map_err(|_| Failure::AdapterInvariant)?
                .take();
            cleanup(kind, deadline, observed).await
        },
    )
    .await;
    outcome.completed_without_failure()
}

async fn cleanup(
    kind: ExitKind,
    deadline: Instant,
    observation: Option<(Arc<Lifecycle>, PathBuf)>,
) -> Result<(), Failure> {
    if let Some((lifecycle, _)) = &observation {
        lifecycle.request_shutdown(match kind {
            ExitKind::Completed | ExitKind::ShutdownRequested => CompletionCause::Success,
            _ => CompletionCause::RuntimeTerminated,
        });
    }
    light_lambda_runtime::validator_ipc::ownership::Drain::process()
        .shutdown(deadline)
        .await?;
    light_lambda_runtime::init::PreparationGate::process()
        .settled(deadline)
        .await
        .map_err(|_| Failure::AdapterInvariant)?;
    if let Some((lifecycle, path)) = observation {
        let observed = lifecycle
            .wait_idle(deadline)
            .await
            .map_err(|_| Failure::AdapterInvariant)?;
        if !observed.clean() {
            return Err(Failure::AdapterInvariant);
        }
        let evidence = serde_json::json!({"evidenceOrigin":"qualificationEntryOwner",
            "lifecycleClean":observed.clean(),"observedChildren":observed.children.len(),
            "precisionDrained":true,"preparationSettled":true,"resourceBoundsProved":false});
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.with_extension("drain.json"))
            .map_err(|_| Failure::AdapterInvariant)?;
        serde_json::to_writer(file, &evidence).map_err(|_| Failure::AdapterInvariant)?;
    }
    Ok(())
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
pub fn trusted_input(
    dir: Option<PathBuf>,
    running: bool,
    engine: Arc<dyn light_lambda_runtime::validator_ipc::Engine>,
    timeout: Duration,
) -> Result<light_lambda_runtime::init::InitInput, light_lambda_runtime::init_error::InitError> {
    let mut input = light_lambda::input(dir, running)?;
    let provider = Arc::new(
        light_lambda_runtime::validator_policy::ValidatorPolicyProvider::with_engine(
            engine,
            Arc::new(light_lambda_runtime::validator_failure_policy::ValidatorFailureDisposition),
            timeout,
        )
        .with_required_profile(required_profile()),
    );
    for id in [
        serverless_core::HandlerId::Specification,
        serverless_core::HandlerId::Validator,
    ] {
        input
            .handlers
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or(
                light_lambda_runtime::init_error::InitError::InvalidConfiguration(
                    "validator: application registration missing",
                ),
            )?
            .factory = Some(provider.clone());
    }
    Ok(input)
}
