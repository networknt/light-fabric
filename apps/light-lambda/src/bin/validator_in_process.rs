//! Separately identified local in-process Validator candidate. Experimental
//! requested-buffer limits only; this is not the accepted release bootstrap.
use light_lambda::{Command, embedded_config, parse_args, phase1_guard};

#[tokio::main]
async fn main() {
    light_lambda_runtime::telemetry::install_panic_redaction();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    match command {
        Command::Run => {
            if light_lambda_runtime::runtime::run_initialized_with_guard(
                || input(None, true),
                Some(phase1_guard()),
            )
            .await
            .is_err()
            {
                std::process::exit(1);
            }
        }
        Command::Validate(dir) => {
            let report = match input(dir, false) {
                Ok(input) => light_lambda_runtime::init::validate_configuration(&input).await,
                Err(error) => light_lambda_runtime::init::ValidationReport {
                    valid: false,
                    errors: vec![error.message()],
                    warnings: vec![],
                    config_digest: None,
                    handlers: vec![],
                    routes: vec![],
                },
            };
            println!(
                "{}",
                serde_json::to_string(&report).expect("static report serializes")
            );
            if !report.valid {
                std::process::exit(1);
            }
        }
        Command::Print => {
            for file in embedded_config::FILES {
                println!("--- {} ---", file.name);
                if config_loader::handle_embedded_config_args(
                    embedded_config::FILES,
                    ["--print-default-config", file.name],
                )
                .is_err()
                {
                    eprintln!("embedded configuration could not be printed");
                    std::process::exit(1);
                }
            }
        }
        Command::Dump(dir) => {
            if config_loader::handle_embedded_config_args(
                embedded_config::FILES,
                [
                    "--dump-default-configs".into(),
                    dir.to_string_lossy().into_owned(),
                ],
            )
            .is_err()
            {
                eprintln!("embedded configuration could not be written");
                std::process::exit(1);
            }
        }
    }
}

fn input(
    dir: Option<std::path::PathBuf>,
    running: bool,
) -> Result<light_lambda_runtime::init::InitInput, light_lambda_runtime::init_error::InitError> {
    light_lambda::input_with_in_process_validator(
        dir,
        running,
        light_lambda_runtime::validator_ipc::in_process::Limits {
            buffer_bytes: 64 << 20,
            body_depth: 128,
            metadata: light_lambda_runtime::validator_ipc::accounting::MetadataLimits {
                max_wire_bytes: 2 << 20,
                max_peak_bytes: 32 << 20,
                max_retained_bytes: 16 << 20,
            },
        },
        std::time::Duration::from_secs(15),
    )
}
