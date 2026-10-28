//! App-owned subprocess entry for the two existing JWKS gate fixtures.
//! This is not a precision worker and adds no fixture or expectation.
#[tokio::test]
#[ignore = "explicitly invoked by the conformance runner with owned dependencies"]
async fn component_probe() {
    let input = std::env::var("LIGHT_LAMBDA_GATE_INPUT").expect("owned gate input");
    let data: serde_json::Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let dir = std::path::Path::new(data["config"].as_str().unwrap());
    light_lambda_runtime::telemetry::install_panic_redaction();
    let mut input = light_lambda::input(Some(dir.into()), false).unwrap();
    input.local_dir = dir.join("no-local-config");
    input.default_dir = None;
    input.environment.allow_insecure_test_endpoints = true;
    let config = light_lambda_runtime::init::prepare_local(&input)
        .await
        .unwrap();
    light_security::run_jwks_gate_component_for_conformance(&config, data).await;
}
