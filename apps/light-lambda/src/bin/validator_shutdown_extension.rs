//! Explicit external extension target. Never included in the ordinary baseline ZIP.
use light_lambda_runtime::qualification_extension_bridge;
use std::process::ExitCode;

fn main() -> ExitCode {
    light_lambda_runtime::telemetry::install_panic_redaction();
    // SAFETY: this dedicated helper sets its mask before creating its single-thread runtime.
    // No ordinary application process or already running threads are affected.
    unsafe { qualification_extension_bridge::set_external_process_mask() };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return refused(),
    };
    let Some(authority) = std::env::var("AWS_LAMBDA_RUNTIME_API").ok() else {
        return refused();
    };
    if runtime
        .block_on(qualification_extension_bridge::extension(&authority))
        .is_ok()
    {
        ExitCode::SUCCESS
    } else {
        refused()
    }
}

fn refused() -> ExitCode {
    eprintln!("validator: shutdown bridge unavailable");
    ExitCode::FAILURE
}
