//! Read-only static validation on the existing authenticated native MCP path.
use crate::rule_api::RuleApiState;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use workflow_expression::{
    DefinitionValidation, Diagnostic, Profile, ValidationError, WorkerError,
};

pub(crate) const INTERFACE: &str = "workflow-definition-validation-v1";
pub(crate) const VALIDATOR: &str = "workflow-admission-v1";
pub(crate) const CONTRACT: &str =
    "a2afdd55b5c5208c9f3c80c0cac4cbad5ff7e03e58d972fee59a78d494d6d72d";
pub(crate) const BUILD: &str = env!("WORKFLOW_VALIDATOR_BUILD");
// Axum's existing JSON transport default is 2 MiB. Apply the same source bound
// to in-repository native dispatch too; no request can increase it.
const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    interface_version: String,
    validator_version: String,
    contract_digest: String,
    host_id: Uuid,
    wf_def_id: Uuid,
    definition: String,
    expected_profile: String,
    definition_source_sha256: String,
}

fn failure(
    status: StatusCode,
    code: &str,
    message: &str,
    details: Value,
    retryable: bool,
) -> Box<Response> {
    Box::new((status,Json(json!({"code":code,"message":message,"details":details,"retryable":retryable,"afterEffect":false}))).into_response())
}
fn bad() -> Box<Response> {
    failure(
        StatusCode::BAD_REQUEST,
        "WORKFLOW_INPUT_INVALID",
        "invalid definition-validation request",
        json!({}),
        false,
    )
}
fn identifier(value: &str) -> &str {
    if value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
    {
        value
    } else {
        "<invalid>"
    }
}
fn unsupported(request: &Request) -> Box<Response> {
    failure(
        StatusCode::BAD_REQUEST,
        "EVALUATOR_PROFILE_UNSUPPORTED",
        "definition-validator compatibility mismatch",
        json!({
            "expected":{"validatorVersion":VALIDATOR,"contractDigest":CONTRACT,"profileId":"cel-workflow-v2"},
            "actual":{"validatorVersion":identifier(&request.validator_version),"contractDigest":identifier(&request.contract_digest),"profileId":identifier(&request.expected_profile)}
        }),
        false,
    )
}
fn diagnostic(request: &Request, diagnostic: Diagnostic) -> Box<Response> {
    failure(
        StatusCode::BAD_REQUEST,
        diagnostic.error.category.code(),
        "authored definition failed static validation",
        json!({"diagnostics":[{
            "code":diagnostic.error.category.code(),"phase":"admission","hostId":request.host_id,"wfDefId":request.wf_def_id,
            "taskName":diagnostic.task,"field":diagnostic.field,"spanIndex":diagnostic.error.span_index,"sourceOffset":diagnostic.error.offset,
        }]}),
        false,
    )
}
fn unavailable(_error: WorkerError) -> Box<Response> {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "WORKFLOW_VALIDATION_UNAVAILABLE",
        "workflow definition validation is unavailable",
        json!({}),
        true,
    )
}
pub(crate) async fn validate(
    state: &RuleApiState,
    headers: &HeaderMap,
    arguments: Value,
) -> Result<Value, Box<Response>> {
    // Existing path verifies both delegated user purpose and trusted Gateway scope
    // identity. No policy row, definition, event or dispatch is read/written here.
    let (identity, generation) = crate::rule_api::authenticate_invoke(state, headers)
        .await
        .map_err(|error| Box::new(error.into_response()))?;
    let request: Request = serde_json::from_value(arguments).map_err(|_| bad())?;
    if request.host_id != identity.host_id {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "WORKFLOW_POLICY_DENIED",
            "request host does not match verified identity",
            json!({}),
            false,
        ));
    }
    if request.interface_version != INTERFACE {
        return Err(bad());
    }
    if request.validator_version != VALIDATOR
        || request.contract_digest != CONTRACT
        || request.expected_profile != "cel-workflow-v2"
    {
        return Err(unsupported(&request));
    }
    if request.definition.len() > SOURCE_BYTES
        || hex::encode(Sha256::digest(request.definition.as_bytes()))
            != request.definition_source_sha256
    {
        return Err(bad());
    }
    let raw: Value = serde_yaml::from_str(&request.definition).map_err(|_| bad())?;
    if !raw.is_object() {
        return Err(bad());
    }
    let plan = DefinitionValidation::from_raw(&raw).map_err(|d| diagnostic(&request, d))?;
    if plan.profile != Profile::CelWorkflowV2 {
        return Err(unsupported(&request));
    }
    // Raw unsupported fields/site shapes were inspected before typed conversion.
    let typed: workflow_core::models::workflow::WorkflowDefinition =
        serde_json::from_value(raw.clone()).map_err(|_| bad())?;
    crate::runtime_definition::validate_runtime_definition(
        &typed,
        generation.config.maximum_parallelism,
    )
    .map_err(|_| {
        diagnostic(
            &request,
            Diagnostic {
                error: workflow_expression::ExpressionError {
                    category: workflow_expression::Category::Unsupported,
                    phase: workflow_expression::Phase::Admission,
                    span_index: None,
                    offset: None,
                },
                task: None,
                field: String::new(),
            },
        )
    })?;
    let digest = crate::rule_api::saved_definition_digest(&raw).map_err(|_| bad())?;
    let engine = state
        .definition_validator
        .as_ref()
        .ok_or_else(|| unavailable(WorkerError::Unavailable))?;
    match tokio::time::timeout(VALIDATION_TIMEOUT, plan.validate(engine)).await {
        Ok(Ok(())) => (),
        Ok(Err(ValidationError::Diagnostic(d))) => return Err(diagnostic(&request, d)),
        Ok(Err(ValidationError::Worker(error))) => return Err(unavailable(error)),
        Err(_) => return Err(unavailable(WorkerError::Unavailable)),
    }
    Ok(
        json!({"valid":true,"interfaceVersion":INTERFACE,"validatorVersion":VALIDATOR,"contractDigest":CONTRACT,
        "validatorBuild":BUILD,"profileId":"cel-workflow-v2","hostId":request.host_id,"wfDefId":request.wf_def_id,
        "definitionSourceSha256":request.definition_source_sha256,"definitionDigest":digest,
        "supportedProfiles":["cel-workflow-v2"],"diagnostics":[]}),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use workflow_expression::WorkerConfig;
    const KEY: &[u8] = b"step03-publisher-signing-key-32-bytes";
    fn signed(claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("step03".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(KEY),
        )
        .unwrap()
    }
    pub(crate) async fn fixture() -> (RuleApiState, HeaderMap, Value) {
        let host = Uuid::new_v4();
        let mut state = RuleApiState::for_publication_test(
            light_security::SecurityRuntime::with_test_hs256_key("step03", KEY).await,
            host,
        );
        state.definition_validator = Some(
            crate::expression_test_support::engine(WorkerConfig {
                workers: 1,
                cache_entries: 8,
                cache_bytes: 128 * 1024,
                ..WorkerConfig::default()
            })
            .unwrap(),
        );
        let mut headers = HeaderMap::new();
        let user = signed(json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "token_use":"user","sub":Uuid::new_v4(),"uid":Uuid::new_v4(),"client_id":"publisher-a","host":host}));
        let scope = signed(json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "sid":"gateway-a","host":host,"env":"dev"}));
        headers.insert("authorization", format!("Bearer {user}").parse().unwrap());
        headers.insert("x-scope-token", format!("Bearer {scope}").parse().unwrap());
        let source = json!({"document":{"dsl":"1.0.3","name":"validation","version":"1.0.0",
            "metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},
            "do":[{"t":{"set":{"x":"${ context.missing }"}}}]})
        .to_string();
        let request = json!({"interfaceVersion":INTERFACE,"validatorVersion":VALIDATOR,"contractDigest":CONTRACT,
            "hostId":host,"wfDefId":Uuid::new_v4(),"definition":source,"expectedProfile":"cel-workflow-v2",
            "definitionSourceSha256":hex::encode(Sha256::digest(source.as_bytes()))});
        (state, headers, request)
    }
    #[tokio::test]
    async fn denies_authority_and_closed_envelope_before_enqueue() {
        let _expression_fixture = crate::expression_test_support::acquire().await;
        let (state, headers, request) = fixture().await;
        for absent in ["authorization", "x-scope-token"] {
            let mut rejected = headers.clone();
            rejected.remove(absent);
            assert!(validate(&state, &rejected, request.clone()).await.is_err());
        }
        let mut app = headers.clone();
        app.insert(
            "authorization",
            format!(
                "Bearer {}",
                signed(json!({"iss":"step03","aud":"workflow",
            "exp":4102444800u64,"token_use":"app","client_id":"publisher-a","uid":Uuid::new_v4(),
            "host":request["hostId"]}))
            )
            .parse()
            .unwrap(),
        );
        assert!(validate(&state, &app, request.clone()).await.is_err());
        let mut wrong_gateway = headers.clone();
        wrong_gateway.insert(
            "x-scope-token",
            format!(
                "Bearer {}",
                signed(json!({"iss":"step03","aud":"workflow",
            "exp":4102444800u64,"sid":"wrong-gateway","host":request["hostId"],"env":"dev"}))
            )
            .parse()
            .unwrap(),
        );
        assert!(
            validate(&state, &wrong_gateway, request.clone())
                .await
                .is_err()
        );
        for (field, value) in [
            ("hostId", json!(Uuid::new_v4())),
            ("extra", json!(true)),
            ("interfaceVersion", json!("v2")),
            ("validatorVersion", json!("v2")),
            ("contractDigest", json!("wrong")),
            ("expectedProfile", json!("legacy-v1")),
            ("definitionSourceSha256", json!("0".repeat(64))),
        ] {
            let mut rejected = request.clone();
            rejected[field] = value;
            assert!(
                validate(&state, &headers, rejected).await.is_err(),
                "{field}"
            );
        }
        for field in request.as_object().unwrap().keys() {
            let mut rejected = request.clone();
            rejected.as_object_mut().unwrap().remove(field);
            assert!(
                validate(&state, &headers, rejected).await.is_err(),
                "missing {field}"
            );
        }
        assert_eq!(
            state
                .definition_validator
                .as_ref()
                .unwrap()
                .compilation_requests(),
            0
        );
        for source in ["not a definition", "{", "---\n{}\n---\n{}", "{}"] {
            let mut rejected = request.clone();
            rejected["definition"] = json!(source);
            rejected["definitionSourceSha256"] =
                json!(hex::encode(Sha256::digest(source.as_bytes())));
            assert!(validate(&state, &headers, rejected).await.is_err());
        }
        let source = request["definition"]
            .as_str()
            .unwrap()
            .replace("cel-workflow-v2", "legacy-v1");
        let mut rejected = request.clone();
        rejected["definition"] = json!(source);
        rejected["definitionSourceSha256"] = json!(hex::encode(Sha256::digest(source.as_bytes())));
        assert!(validate(&state, &headers, rejected).await.is_err());
        assert_eq!(
            state
                .definition_validator
                .as_ref()
                .unwrap()
                .compilation_requests(),
            0
        );
    }
    #[tokio::test]
    async fn native_validation_is_static_without_policy_or_store_and_hashes_exact_bytes() {
        let _expression_fixture = crate::expression_test_support::acquire().await;
        let (state, headers, mut request) = fixture().await;
        let first = crate::rule_api::dispatch_native_tool(
            "workflow_definition_validate",
            state.clone(),
            headers.clone(),
            request.clone(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(first["valid"], true);
        assert_eq!(first["diagnostics"], json!([]));
        assert_eq!(
            first["definitionSourceSha256"],
            request["definitionSourceSha256"]
        );
        let raw: Value = serde_json::from_str(request["definition"].as_str().unwrap()).unwrap();
        assert_eq!(
            first["definitionDigest"],
            crate::rule_api::saved_definition_digest(&raw).unwrap()
        );
        assert!(first["validatorBuild"].as_str().unwrap().strip_prefix("workflow-admission-source-sha256:")
            .is_some_and(|hash| hash.len()==64 && hash.bytes().all(|b| b.is_ascii_hexdigit())));
        let source = serde_json::to_string_pretty(&raw).unwrap();
        request["definition"] = json!(source);
        request["definitionSourceSha256"] = json!(hex::encode(Sha256::digest(source.as_bytes())));
        let second = validate(&state, &headers, request).await.unwrap();
        assert_eq!(first["definitionDigest"], second["definitionDigest"]);
        assert_ne!(
            first["definitionSourceSha256"],
            second["definitionSourceSha256"]
        );
        state
            .definition_validator
            .as_ref()
            .unwrap()
            .shutdown(std::time::Duration::from_secs(2))
            .unwrap();
        let (_, _, request) = fixture().await;
        // Retain the original authenticated host while exercising a stopped pool.
        let mut request = request;
        request["hostId"] = first["hostId"].clone();
        assert_eq!(
            validate(&state, &headers, request)
                .await
                .unwrap_err()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // The fixture has no reachable database and no operational policy row.
        // Missing runtime context would fail evaluation; static validation succeeds.
    }
    #[tokio::test]
    async fn diagnostics_are_sanitized_and_unavailable_is_retryable() {
        let _expression_fixture = crate::expression_test_support::acquire().await;
        let (mut state, headers, mut request) = fixture().await;
        let source = request["definition"]
            .as_str()
            .unwrap()
            .replace("context.missing", "private_runtime_secret");
        request["definition"] = json!(source);
        request["definitionSourceSha256"] = json!(hex::encode(Sha256::digest(source.as_bytes())));
        let response = validate(&state, &headers, request.clone())
            .await
            .unwrap_err();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["details"]["diagnostics"][0]["field"], "/do/0/t/set/x");
        assert_eq!(body["details"]["diagnostics"][0]["taskName"], "t");
        assert_eq!(body["details"]["diagnostics"][0]["sourceOffset"], 2);
        assert!(
            !String::from_utf8(bytes.to_vec())
                .unwrap()
                .contains("private_runtime_secret")
        );
        state.definition_validator = None;
        assert_eq!(
            validate(&state, &headers, request)
                .await
                .unwrap_err()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        for error in [
            WorkerError::Unavailable,
            WorkerError::IncompleteCleanup,
            WorkerError::Full,
        ] {
            let response = unavailable(error);
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["retryable"], true);
            assert_eq!(body["afterEffect"], false);
        }
    }
}
