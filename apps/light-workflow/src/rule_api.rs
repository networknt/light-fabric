use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use light_rule::{ActionRegistry, Rule, RuleEngine};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Clone)]
pub struct RuleApiState {
    engine: Arc<RuleEngine>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTestRequest {
    pub rule_body: Value,
    pub input_context: Value,
    pub expected_result: Option<bool>,
    pub test_mode: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTestResponse {
    pub executor: String,
    pub passed: bool,
    pub expected_result: Option<bool>,
    pub success: bool,
    pub mutated_context: Value,
}

pub async fn run_rule_api() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var("LIGHT_WORKFLOW_HTTP_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;

    let state = RuleApiState {
        engine: Arc::new(RuleEngine::new(Arc::new(ActionRegistry::new()))),
    };

    let app = Router::new()
        .route("/rule/test", post(run_rule_test))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Light Workflow rule test API listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn run_rule_test(
    State(state): State<RuleApiState>,
    Json(request): Json<RuleTestRequest>,
) -> Result<Json<RuleTestResponse>, (StatusCode, Json<Value>)> {
    let mut rule: Rule = serde_json::from_value(request.rule_body).map_err(bad_request)?;
    if request
        .test_mode
        .as_deref()
        .unwrap_or("conditions")
        .eq_ignore_ascii_case("conditions")
    {
        rule.actions = None;
    }

    let mut context = request.input_context;
    let passed = state
        .engine
        .execute_rule(&rule, &mut context)
        .await
        .map_err(|err| {
            error!("Rust rule test failed for {}: {}", rule.rule_id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Rule engine failed: {}", err) })),
            )
        })?;

    let success = request
        .expected_result
        .map_or(true, |expected| expected == passed);
    Ok(Json(RuleTestResponse {
        executor: "rust".to_string(),
        passed,
        expected_result: request.expected_result,
        success,
        mutated_context: context,
    }))
}

fn bad_request<E: std::fmt::Display>(err: E) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}
