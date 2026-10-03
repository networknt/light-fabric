//! V2 immutable acceptance identity. Recovery never performs executable work.
// Keep the established Workflow error envelope at the new boundary.
#![allow(clippy::result_large_err)]
#[cfg(test)]
#[path = "operational_admission_postgres.rs"]
mod postgres_gates;
use crate::rule_api::{ApiError, InvocationIdentity, RuleApiState};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
use workflow_expression::{DefinitionValidation, Profile};
use workflow_invocation_contract::{ErrorCode, InvocationStatus};

pub(crate) const RECEIPT_INTERFACE: &str = "workflow-start-receipt-v1";

pub(crate) fn canonical(value: &Value) -> Result<String, ApiError> {
    fn append(value: &Value, out: &mut String) -> Result<(), ApiError> {
        match value {
            Value::Object(values) => {
                out.push('{');
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(
                        &serde_json::to_string(key)
                            .map_err(|_| ApiError::input_invalid("invalid exact request JSON"))?,
                    );
                    out.push(':');
                    append(&values[key], out)?;
                }
                out.push('}');
            }
            Value::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    append(value, out)?;
                }
                out.push(']');
            }
            // Revision/budget/options retain full i64/u64 integer identities.
            // Start input itself still uses its existing safe-number contract.
            Value::Number(number) if !(number.is_i64() || number.is_u64()) => {
                return Err(ApiError::input_invalid("invalid exact request JSON"));
            }
            _ => out.push_str(
                &serde_json::to_string(value)
                    .map_err(|_| ApiError::input_invalid("invalid exact request JSON"))?,
            ),
        }
        Ok(())
    }
    let mut out = String::new();
    append(value, &mut out)?;
    Ok(out)
}
fn digest(text: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes())))
}

#[derive(Clone, Debug)]
pub(crate) struct Operation {
    host: Uuid,
    principal: String,
    end_user: String,
    pub(crate) kind: String,
    key: String,
    exact: String,
    pub(crate) request_digest: String,
}
impl Operation {
    pub(crate) fn new(
        identity: &InvocationIdentity,
        kind: &str,
        key: &str,
        request: &Value,
    ) -> Result<Self, ApiError> {
        if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
            return Err(ApiError::input_invalid("invalid operation key"));
        }
        let exact = canonical(request)?;
        Ok(Self {
            host: identity.host_id,
            principal: identity.principal_subject.clone(),
            end_user: identity.end_user_subject.clone(),
            kind: kind.into(),
            key: key.into(),
            request_digest: digest(&exact),
            exact,
        })
    }
    pub(crate) fn scope(&self) -> String {
        // Neither definition digest nor request content partitions the caller key.
        digest(
            &serde_json::to_string(&json!([
                self.host,
                self.principal,
                self.end_user,
                self.kind,
                self.key
            ]))
            .expect("plain JSON"),
        )
    }
    pub(crate) async fn lock(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(self.scope())
            .execute(&mut **tx)
            .await
            .map_err(ApiError::database)?;
        Ok(())
    }
    pub(crate) async fn recover(&self, pool: &PgPool) -> Result<Option<Committed>, ApiError> {
        self.recover_with_authority(pool, true).await
    }
    pub(crate) async fn discover(&self, pool: &PgPool) -> Result<Option<Committed>, ApiError> {
        self.recover_with_authority(pool, false).await
    }
    async fn recover_with_authority(
        &self,
        pool: &PgPool,
        authoritative: bool,
    ) -> Result<Option<Committed>, ApiError> {
        // Legacy entry points also probe for v2 identities. Absence of the new
        // forward migration does not change their historical contracts.
        let present: bool = sqlx::query_scalar(
            "SELECT to_regclass('workflow_ops.workflow_operation_receipt_t') IS NOT NULL",
        )
        .fetch_one(pool)
        .await
        .map_err(ApiError::database)?;
        if !present {
            return Ok(None);
        }
        self.lookup_with_authority(pool, authoritative).await
    }
    pub(crate) async fn locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<Committed>, ApiError> {
        self.lookup(&mut **tx).await
    }
    async fn lookup<'e, E: sqlx::Executor<'e, Database = Postgres>>(
        &self,
        executor: E,
    ) -> Result<Option<Committed>, ApiError> {
        self.lookup_with_authority(executor, true).await
    }
    async fn lookup_with_authority<'e, E: sqlx::Executor<'e, Database = Postgres>>(
        &self,
        executor: E,
        authoritative: bool,
    ) -> Result<Option<Committed>, ApiError> {
        let row: Option<(String,String,Value,Option<Value>)> = sqlx::query_as(
            "SELECT request_text,request_digest,receipt,invocation_status FROM workflow_ops.workflow_operation_receipt_t WHERE host_id=$1 AND principal_subject=$2 AND end_user_subject=$3 AND operation_kind=$4 AND caller_key=$5")
            .bind(self.host).bind(&self.principal).bind(&self.end_user).bind(&self.kind).bind(&self.key)
            .fetch_optional(executor).await.map_err(ApiError::database)?;
        match row {
            Some((exact, hash, receipt, status)) => {
                self.match_recovery(&exact, &hash, receipt, status, authoritative)
            }
            None => Ok(None),
        }
    }
    pub(crate) fn match_recovery(
        &self,
        exact: &str,
        hash: &str,
        receipt: Value,
        status: Option<Value>,
        authoritative: bool,
    ) -> Result<Option<Committed>, ApiError> {
        // Discovery cannot establish which binding policy selected this key.
        // A different request may belong to an unrelated explicit operation.
        if !authoritative && (exact != self.exact || hash != self.request_digest) {
            return Ok(None);
        }
        self.match_committed(exact, hash, receipt, status).map(Some)
    }
    fn match_committed(
        &self,
        exact: &str,
        hash: &str,
        receipt: Value,
        status: Option<Value>,
    ) -> Result<Committed, ApiError> {
        if exact != self.exact || hash != self.request_digest {
            return Err(conflict());
        }
        Ok(Committed {
            receipt,
            status: status
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| unavailable())?,
        })
    }
    pub(crate) async fn store(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        selected: &Value,
        receipt: &Value,
        status: Option<&InvocationStatus>,
    ) -> Result<(), ApiError> {
        sqlx::query("INSERT INTO workflow_ops.workflow_operation_receipt_t(host_id,principal_subject,end_user_subject,operation_kind,caller_key,request_text,request_digest,selected_content,receipt,invocation_status) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(self.host).bind(&self.principal).bind(&self.end_user).bind(&self.kind).bind(&self.key)
            .bind(&self.exact).bind(&self.request_digest).bind(selected).bind(receipt)
            .bind(status.map(serde_json::to_value).transpose().map_err(|_| unavailable())?)
            .execute(&mut **tx).await.map_err(ApiError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> InvocationIdentity {
        InvocationIdentity {
            host_id: Uuid::from_u128(1),
            principal_subject: "principal".into(),
            end_user_subject: "user".into(),
            caller_claims_digest: "unused".into(),
            caller_claims: json!({}),
            user_authorization: "unused".into(),
            user_authorization_exp: 0,
        }
    }
    #[test]
    fn shared_rust_java_canonical_fixtures() {
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../contracts/workflow-admin/receipt-canonical-fixtures.json"
        ))
        .unwrap();
        for fixture in fixtures {
            let value: Value = serde_json::from_str(fixture["json"].as_str().unwrap()).unwrap();
            if fixture["invalid"] == true {
                assert!(canonical(&value).is_err(), "{}", fixture["name"]);
            } else {
                let exact = canonical(&value).unwrap();
                assert_eq!(
                    exact,
                    fixture["canonical"].as_str().unwrap(),
                    "{}",
                    fixture["name"]
                );
                assert_eq!(digest(&exact), fixture["digest"].as_str().unwrap());
            }
        }
    }
    #[test]
    fn changed_content_cannot_partition_key_and_receipt_is_original() {
        let identity = identity();
        let request = json!({"expectedDefinitionDigest":"old","input":{"x":1}});
        let original = Operation::new(&identity, "workflow_start", "caller-key", &request).unwrap();
        for changed in [
            json!({"expectedDefinitionDigest":"new","input":{"x":1}}),
            json!({"expectedDefinitionDigest":"old","input":{"x":2}}),
        ] {
            let retry =
                Operation::new(&identity, "workflow_start", "caller-key", &changed).unwrap();
            assert_eq!(original.scope(), retry.scope());
            assert_eq!(
                axum::response::IntoResponse::into_response(
                    retry
                        .match_committed(
                            &original.exact,
                            &original.request_digest,
                            json!({"original":true}),
                            None
                        )
                        .err()
                        .unwrap()
                )
                .status(),
                StatusCode::CONFLICT
            );
        }
        let receipt = json!({"acceptedAt":"original","replayed":false,"state":"ACCEPTED"});
        assert_eq!(
            original
                .match_committed(
                    &original.exact,
                    &original.request_digest,
                    receipt.clone(),
                    None
                )
                .unwrap()
                .receipt,
            receipt
        );
        let mut other = identity;
        other.end_user_subject = "another".into();
        assert_ne!(
            original.scope(),
            Operation::new(&other, "workflow_start", "caller-key", &request)
                .unwrap()
                .scope()
        );
    }
    #[test]
    fn exact_identity_preserves_field_presence_and_closed_recovery_input() {
        assert_ne!(
            canonical(&json!({"input":{}})).unwrap(),
            canonical(&json!({"input":{},"expectedDefinitionDigest":null})).unwrap()
        );
        assert!(serde_json::from_value::<RecoveryRequest>(json!({"interfaceVersion":RECEIPT_INTERFACE,"originalRequest":{},"hostId":"caller-override"})).is_err());
    }
    async fn response_code(error: ApiError) -> (StatusCode, String) {
        use axum::response::IntoResponse;
        let response = error.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        (status, json["code"].as_str().unwrap().to_owned())
    }
    #[tokio::test]
    async fn profile_field_and_availability_errors_preserve_legacy() {
        let _expression_fixture = crate::expression_test_support::acquire().await;
        let security = light_security::SecurityRuntime::with_test_hs256_key(
            "w3c-test",
            b"public-w3c-test-fixture-key-32-bytes",
        )
        .await;
        let mut state = RuleApiState::for_publication_test(security, Uuid::from_u128(1));
        let raw = json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"t":{"set":{"x":"${ 1 }"}}}]});
        assert_eq!(
            response_code(validate(&state, raw.clone()).await.err().unwrap()).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "WORKFLOW_INVOCATION_UNAVAILABLE".into()
            )
        );
        let mut unknown = raw.clone();
        unknown["document"]["metadata"]["lightExpressionProfile"] = json!("unknown");
        assert_eq!(
            response_code(validate(&state, unknown).await.err().unwrap())
                .await
                .1,
            "EVALUATOR_PROFILE_UNSUPPORTED"
        );
        let mut rejected = raw;
        rejected["output"] = json!({"schema":{}});
        let engine = crate::expression_test_support::engine(workflow_expression::WorkerConfig {
            workers: 1,
            ..Default::default()
        })
        .unwrap();
        state.definition_validator = Some(engine.clone());
        assert_eq!(
            response_code(validate(&state, rejected.clone()).await.err().unwrap())
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        rejected["document"]["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("lightExpressionProfile");
        assert_eq!(
            validate(&state, rejected).await.unwrap().profile,
            Profile::LegacyV1
        );
        drop(state);
        tokio::task::spawn_blocking(move || engine.shutdown(std::time::Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn alternate_verified_definition_writers_cannot_bypass_v2_worker_and_policy_boundary() {
        let pool = PgPool::connect_lazy("postgres://unused:unused@127.0.0.1:1/unused").unwrap();
        let raw = json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"t":{"set":{"x":"${ 1 }"}}}]});
        let args = json!({"definition":raw.to_string()});
        let save = crate::publication_api::save_definition_verified(&pool, &args)
            .await
            .err()
            .unwrap();
        assert_eq!(response_code(save).await.1, "EVALUATOR_PROFILE_UNSUPPORTED");
        let called = std::sync::atomic::AtomicBool::new(false);
        let compile = |_: &str| {
            called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let publish = crate::publication_api::publish_definition_verified(
            &pool,
            &args,
            "verified-user",
            &[],
            &compile,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            response_code(publish).await.1,
            "EVALUATOR_PROFILE_UNSUPPORTED"
        );
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }
}
pub(crate) struct Committed {
    pub(crate) receipt: Value,
    pub(crate) status: Option<InvocationStatus>,
}
pub(crate) fn conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        ErrorCode::WorkflowIdempotencyConflict,
        "operation key was used with different exact content",
    )
}
pub(crate) fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::WorkflowInvocationUnavailable,
        "exact acceptance evidence is unavailable",
    )
}
pub(crate) fn unsupported() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::EvaluatorProfileUnsupported,
        "EVALUATOR_PROFILE_UNSUPPORTED",
    )
}
fn diagnostic(d: workflow_expression::Diagnostic) -> ApiError {
    use workflow_expression::Category;
    let code = match d.error.category {
        Category::ProfileUnsupported => ErrorCode::EvaluatorProfileUnsupported,
        Category::Unsupported => ErrorCode::ExpressionUnsupported,
        Category::Limit => ErrorCode::ExpressionLimit,
        Category::JsonProfile => ErrorCode::ExpressionJsonProfile,
        Category::Invalid => ErrorCode::ExpressionInvalid,
        Category::Evaluation => ErrorCode::ExpressionEvaluation,
        Category::ResultType => ErrorCode::ExpressionResultType,
    };
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, d.error.category.code())
        .with_details(json!({"task":d.task,"field":d.field,"spanIndex":d.error.span_index,"sourceOffset":d.error.offset}))
}
pub(crate) fn profile(raw: &Value) -> Result<Profile, ApiError> {
    workflow_expression::resolve_profile(raw).map_err(|_| unsupported())
}
#[derive(Debug)]
pub struct Validated {
    pub(crate) raw: Value,
    pub(crate) profile: Profile,
}
pub(crate) async fn validate(state: &RuleApiState, raw: Value) -> Result<Validated, ApiError> {
    let plan = DefinitionValidation::from_raw(&raw).map_err(diagnostic)?;
    if plan.profile == Profile::CelWorkflowV2 {
        let engine = state
            .definition_validator
            .as_ref()
            .ok_or_else(unavailable)?;
        match tokio::time::timeout(std::time::Duration::from_secs(5), plan.validate(engine)).await {
            Ok(Ok(())) => (),
            Ok(Err(workflow_expression::ValidationError::Diagnostic(d))) => {
                return Err(diagnostic(d));
            }
            _ => return Err(unavailable()),
        }
    }
    Ok(Validated {
        raw,
        profile: plan.profile,
    })
}
pub(crate) async fn policy(tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
    let rows: Vec<bool> = sqlx::query_scalar("SELECT admission_enabled FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2' FOR SHARE")
        .fetch_all(&mut **tx).await.map_err(ApiError::database)?;
    if rows.as_slice() != [true] {
        return Err(unsupported());
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryRequest {
    interface_version: String,
    original_request: Value,
}
pub(crate) async fn start_receipt(
    state: &RuleApiState,
    headers: &HeaderMap,
    request: Value,
) -> Result<Value, ApiError> {
    let (identity, _) = crate::rule_api::authenticate(state, headers).await?;
    let request: RecoveryRequest = serde_json::from_value(request)
        .map_err(|_| ApiError::input_invalid("invalid start receipt request"))?;
    if request.interface_version != RECEIPT_INTERFACE {
        return Err(ApiError::input_invalid("unsupported receipt interface"));
    }
    let original = crate::rule_api::parse_native_start_input(request.original_request.clone())?;
    let operation = Operation::new(
        &identity,
        "workflow_start",
        &original.idempotency_key,
        &request.original_request,
    )?;
    // Unlike compatibility probes in existing handlers, this versioned lookup
    // cannot report absence when its receipt store is not installed.
    if let Some(committed) = operation.lookup(&state.pool).await? {
        return Ok(
            json!({"interfaceVersion":RECEIPT_INTERFACE,"status":"committed","requestDigest":operation.request_digest,"receipt":committed.receipt}),
        );
    }
    // Historical reservations are not exact evidence and cannot be reconstructed.
    // Old reservations do not retain the original caller key/options. A
    // definition filter could hide changed-content reuse of that key.
    let historical: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_invocation_idempotency_t WHERE host_id=$1 AND principal_subject=$2 AND end_user_subject=$3)")
        .bind(identity.host_id).bind(&identity.principal_subject).bind(&identity.end_user_subject)
        .fetch_one(&state.pool).await.map_err(ApiError::database)?;
    if historical {
        return Err(unavailable());
    }
    Ok(
        json!({"interfaceVersion":RECEIPT_INTERFACE,"status":"notFound","requestDigest":operation.request_digest}),
    )
}
