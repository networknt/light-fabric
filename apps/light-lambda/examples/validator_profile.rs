//! Explicit local qualification executable; default bootstrap remains closed.
//! Its stdin control cancels the runtime future, then awaits actual child-owner
//! cleanup. The precision child itself retains the original controller policy.
use light_lambda_runtime::validator_ipc::{
    controller::ResourceProfile,
    lifecycle::{CompletionCause, Lifecycle, SealedObservedProfileEngine},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
#[tokio::main]
async fn main() {
    light_lambda_runtime::telemetry::install_panic_redaction();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 7 {
        eprintln!(
            "usage: validator_profile CONFIG WORKER PARENT_BYTES WORKER_BYTES AS_BYTES CPU_SECONDS LIFECYCLE_JSONL"
        );
        std::process::exit(2);
    }
    let profile = ResourceProfile {
        parent_bytes: args[2].parse().expect("parent bytes"),
        worker_bytes: args[3].parse().expect("worker bytes"),
        address_space_bytes: args[4].parse().expect("address space bytes"),
        cpu_seconds: args[5].parse().expect("CPU seconds"),
    };
    profile.validate().expect("nonzero resource profile");
    let lifecycle = Lifecycle::create(std::path::Path::new(&args[6]))
        .expect("exclusive lifecycle evidence file");
    let engine = Arc::new(SealedObservedProfileEngine {
        executable: light_lambda_runtime::bundle_snapshot::Pin {
            path: PathBuf::from(&args[1]),
            bytes: 17_343_288,
            sha256: [
                175, 141, 190, 122, 176, 61, 6, 52, 24, 146, 153, 191, 38, 176, 251, 97, 194, 161,
                141, 161, 172, 162, 57, 209, 35, 198, 176, 30, 162, 74, 173, 107,
            ],
        },
        profile,
        lifecycle: lifecycle.clone(),
    });
    let (control_tx, control_rx) = tokio::sync::oneshot::channel();
    // A dedicated std thread avoids an uncancellable Tokio stdin blocking task
    // keeping runtime shutdown alive after an early Init/runtime error.
    std::thread::spawn(move || {
        let mut line = String::new();
        let cause = match std::io::stdin().read_line(&mut line) {
            Ok(_) => CompletionCause::parse(line.trim()).unwrap_or(CompletionCause::ControlClosed),
            Err(_) => CompletionCause::ControlClosed,
        };
        let _ = control_tx.send(cause);
    });
    let mut runtime = Box::pin(light_lambda_runtime::runtime::run_initialized_with_guard(
        || {
            light_lambda::input_with_precision_engine(
                Some(PathBuf::from(&args[0])),
                true,
                engine,
                Duration::from_secs(30),
            )
        },
        Some(light_lambda::phase1_guard()),
    ));
    let cause = tokio::select! {biased;
        cause=control_rx=>cause.unwrap_or(CompletionCause::ControlClosed),
        _=runtime.as_mut()=>CompletionCause::RuntimeTerminated,
    };
    lifecycle.request_shutdown(cause);
    // This drops application Snapshot/session/Actor senders. The existing owner
    // task observes channel closure and performs its own kill + Child::wait.
    drop(runtime);
    if lifecycle
        .wait_idle(tokio::time::Instant::now() + Duration::from_secs(6))
        .await
        .is_err()
    {
        eprintln!("precision child lifecycle evidence incomplete");
        std::process::exit(1);
    }
    if matches!(
        cause,
        CompletionCause::RuntimeTerminated | CompletionCause::ControlClosed
    ) {
        std::process::exit(1);
    }
}
