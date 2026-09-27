//! Portal-authoritative definition publication and grant synchronization.

pub(crate) mod binding;
pub use binding::{
    decide_verified, get_verified, list_verified, pinned_dependencies, pinned_evidence,
    publish_binding_verified, retire_binding_verified, revoke_verified,
};

use axum::http::{HeaderMap, StatusCode};
use light_security::{
    AuthPrincipal, JwtExpiryMode,
    token_purpose::{TokenUse, verify_with_purpose},
    verify_jwt_token,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    action_api::ActionSettings,
    rule_api::{
        ApiError, InvocationIdentity, RuleApiState, publication_database_error as database_error,
    },
};

pub(crate) async fn dispatch(
    name: &str,
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
    cel_validator: &(dyn Fn(&str) -> Result<(), ApiError> + Send + Sync),
) -> Result<Option<Value>, ApiError> {
    match name {
        "workflow_definition_save" => save_definition(state, headers, args, settings)
            .await
            .map(Some),
        "workflow_definition_grants_sync" => {
            sync_grants(state, headers, args, settings).await.map(Some)
        }
        "workflow_definition_publish" => {
            publish_definition(state, headers, args, settings, cel_validator)
                .await
                .map(Some)
        }
        "workflow_definition_retire" => retire_definition(state, headers, args, settings)
            .await
            .map(Some),
        "workflow_binding_publish" => binding::publish(state, headers, args, settings)
            .await
            .map(Some),
        "workflow_binding_retire" => binding::retire(state, headers, args, settings)
            .await
            .map(Some),
        "workflow_binding_get" => binding::get(state, headers, args).await.map(Some),
        "workflow_binding_list" => binding::list(state, headers, args).await.map(Some),
        "workflow_binding_decide" => binding::decide(state, headers, args).await.map(Some),
        "workflow_binding_revoke" => binding::revoke(state, headers, args).await.map(Some),
        _ => Ok(None),
    }
}

fn error(
    status: StatusCode,
    code: workflow_invocation_contract::ErrorCode,
    message: impl Into<String>,
) -> ApiError {
    crate::rule_api::publication_error(status, code, message, None)
}

fn conflict_with_details(message: &str, details: Value) -> ApiError {
    crate::rule_api::publication_error(
        StatusCode::CONFLICT,
        workflow_invocation_contract::ErrorCode::WorkflowIdempotencyConflict,
        message,
        Some(details),
    )
}

fn field<'a>(args: &'a Value, name: &str) -> Result<&'a Value, ApiError> {
    args.get(name).ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            format!("missing {name}"),
        )
    })
}

fn text<'a>(args: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    field(args, name)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                format!("invalid {name}"),
            )
        })
}

fn uuid(args: &Value, name: &str) -> Result<Uuid, ApiError> {
    text(args, name)?.parse().map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            format!("invalid {name}"),
        )
    })
}

fn host_claim(principal: &AuthPrincipal) -> Option<Uuid> {
    principal
        .host
        .as_deref()
        .or_else(|| principal.claims.get("hostId").and_then(Value::as_str))
        .or_else(|| principal.claims.get("host_id").and_then(Value::as_str))
        .and_then(|s| s.parse().ok())
}

// Portal persists `positions` with auth codes/refresh tokens. The issued
// position claim used by the Java authorization path is `pos` (a delimited
// string); accept a structured `positions` claim too when an issuer supplies
// one. Both values come only from the verified user token.
fn verified_positions(claims: &Value) -> Vec<String> {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| collect(value, out)),
            Value::String(value) => out.extend(
                value
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            ),
            _ => {}
        }
    }
    let mut positions = Vec::new();
    for name in ["pos", "positions"] {
        if let Some(value) = claims.get(name) {
            collect(value, &mut positions);
        }
    }
    positions.sort();
    positions.dedup();
    positions
}

fn is_definition_owner(
    user_id: &str,
    positions: &[String],
    owner_user: Option<Uuid>,
    owner_position: Option<&str>,
) -> bool {
    owner_user.is_some_and(|owner| user_id.parse::<Uuid>().ok() == Some(owner))
        || owner_position.is_some_and(|owner| positions.iter().any(|position| position == owner))
}

fn verified_user_id(identity: &InvocationIdentity) -> Result<&str, ApiError> {
    if identity
        .caller_claims
        .get("token_use")
        .and_then(Value::as_str)
        == Some("app")
    {
        return Err(ApiError::policy_denied("a user token is required"));
    }
    identity
        .caller_claims
        .get("user_id")
        .and_then(Value::as_str)
        .or_else(|| identity.caller_claims.get("uid").and_then(Value::as_str))
        .or_else(|| {
            (identity
                .caller_claims
                .get("token_use")
                .and_then(Value::as_str)
                == Some("user"))
            .then(|| identity.caller_claims.get("sub").and_then(Value::as_str))
            .flatten()
        })
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::policy_denied("verified user id is required"))
}

fn publisher_client(principal: &AuthPrincipal) -> Option<&str> {
    principal
        .client_id
        .as_deref()
        .or_else(|| principal.claims.get("cid").and_then(Value::as_str))
        .or_else(|| principal.claims.get("client_id").and_then(Value::as_str))
}

fn assert_publisher_claims(
    principal: &AuthPrincipal,
    host_id: Uuid,
    publisher_client_ids: &[String],
) -> Result<(), ApiError> {
    let client_id = publisher_client(principal)
        .ok_or_else(|| ApiError::policy_denied("publisher token has no client id"))?;
    if !publisher_client_ids.iter().any(|id| id == client_id) {
        return Err(ApiError::policy_denied(
            "publisher client is not allowlisted",
        ));
    }
    if host_claim(principal) != Some(host_id) {
        return Err(ApiError::policy_denied(
            "publisher token host does not match request host",
        ));
    }
    Ok(())
}

fn publisher_header_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("x-publisher-token")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::policy_denied("publisher token is required"))
}

fn bearer<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ApiError> {
    let value = headers
        .get(name)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| ApiError::policy_denied("publisher authorization is required"))?;
    if !scheme.eq_ignore_ascii_case("Bearer") || token.trim().is_empty() {
        return Err(ApiError::policy_denied(
            "publisher authorization is invalid",
        ));
    }
    Ok(token.trim())
}

fn publisher_keys(
    settings: Option<&ActionSettings>,
) -> &[light_security::token_purpose::LegacyLongLivedAppKey] {
    settings
        .map(|s| s.policy.legacy_long_lived_app_keys.as_slice())
        .unwrap_or(&[])
}

async fn verify_publisher_token(
    security: &light_security::SecurityRuntime,
    publisher_client_ids: &[String],
    token: &str,
    host_id: Uuid,
    settings: Option<&ActionSettings>,
) -> Result<AuthPrincipal, ApiError> {
    if publisher_client_ids.is_empty() {
        return Err(ApiError::policy_denied(
            "publisher client is not allowlisted",
        ));
    }
    let principal = verify_with_purpose(security, token, TokenUse::App, publisher_keys(settings))
        .await
        .map_err(|_| ApiError::policy_denied("publisher application token is invalid"))?;
    assert_publisher_claims(&principal, host_id, publisher_client_ids)?;
    Ok(principal)
}

/// Publisher assertion used with the normal user authentication path.
pub(crate) async fn verify_publisher_header(
    state: &RuleApiState,
    headers: &HeaderMap,
    host_id: Uuid,
    settings: Option<&ActionSettings>,
) -> Result<AuthPrincipal, ApiError> {
    let generation = state.runtime_config.load();
    verify_publisher_header_with(
        &state.invocation_security,
        &generation.config.publisher_client_ids,
        headers,
        host_id,
        settings,
    )
    .await
}

async fn verify_publisher_header_with(
    security: &light_security::SecurityRuntime,
    publisher_client_ids: &[String],
    headers: &HeaderMap,
    host_id: Uuid,
    settings: Option<&ActionSettings>,
) -> Result<AuthPrincipal, ApiError> {
    verify_publisher_token(
        security,
        publisher_client_ids,
        publisher_header_token(headers)?,
        host_id,
        settings,
    )
    .await
}

/// Publisher assertion for durable save/grant synchronization. This validates
/// the Gateway service token separately; actor payload data is audit metadata.
pub(crate) async fn authenticate_publisher_service(
    state: &RuleApiState,
    headers: &HeaderMap,
    host_id: Uuid,
    settings: Option<&ActionSettings>,
) -> Result<AuthPrincipal, ApiError> {
    let generation = state.runtime_config.load();
    authenticate_publisher_service_with(
        &state.invocation_security,
        &generation.config.publisher_client_ids,
        &generation.config.invocation_caller_service_ids,
        &generation.config.invocation_caller_environments,
        state.invocation_environment.as_ref(),
        headers,
        host_id,
        settings,
    )
    .await
}

async fn authenticate_publisher_service_with(
    security: &light_security::SecurityRuntime,
    publisher_client_ids: &[String],
    caller_service_ids: &[String],
    caller_environments: &[String],
    invocation_environment: &str,
    headers: &HeaderMap,
    host_id: Uuid,
    settings: Option<&ActionSettings>,
) -> Result<AuthPrincipal, ApiError> {
    let app_token = bearer(headers, "authorization")?;
    let publisher =
        verify_publisher_token(security, publisher_client_ids, app_token, host_id, settings)
            .await?;
    let scope_token = bearer(headers, "x-scope-token")?;
    let scope = verify_jwt_token(security, scope_token, JwtExpiryMode::Enforce)
        .await
        .map_err(|_| ApiError::policy_denied("Gateway caller token is invalid"))?;
    let sid = scope.claims.get("sid").and_then(Value::as_str);
    if sid.is_none_or(|sid| !caller_service_ids.iter().any(|id| id == sid)) {
        return Err(ApiError::policy_denied(
            "Gateway caller service is not allowed",
        ));
    }
    if scope
        .host
        .as_deref()
        .and_then(|value| value.parse::<Uuid>().ok())
        != Some(host_id)
    {
        return Err(ApiError::policy_denied(
            "Gateway caller host does not match request host",
        ));
    }
    let env = scope.claims.get("env").and_then(Value::as_str);
    let env_ok = if caller_environments.is_empty() {
        env == Some(invocation_environment)
    } else {
        env.is_some_and(|env| caller_environments.iter().any(|allowed| allowed == env))
    };
    if !env_ok {
        return Err(ApiError::policy_denied(
            "Gateway caller environment is not allowed",
        ));
    }
    Ok(publisher)
}

fn assert_body_host(args: &Value, host: Uuid) -> Result<(), ApiError> {
    if uuid(args, "hostId")? != host {
        return Err(ApiError::policy_denied(
            "request host does not match verified identity",
        ));
    }
    Ok(())
}

async fn with_matching_host<T, F, Fut>(
    args: &Value,
    verified_host: Uuid,
    scoped: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    assert_body_host(args, verified_host)?;
    scoped().await
}

async fn user_and_publisher(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<InvocationIdentity, ApiError> {
    // User and x-scope tokens are verified before the body host is used in SQL.
    let (identity, _) = crate::rule_api::authenticate(state, headers).await?;
    verified_user_id(&identity)?;
    assert_body_host(args, identity.host_id)?;
    verify_publisher_header(state, headers, identity.host_id, settings).await?;
    Ok(identity)
}

fn digest_value(value: &Value) -> Result<String, ApiError> {
    let digest = execution_runner_protocol::canonical_sha256(value)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    Ok(format!("sha256:{digest}"))
}

/// Digest exactly the parsed YAML JSON value. Object order, whitespace and
/// comments disappear; sequence order, YAML scalar types and explicit nulls remain.
pub(crate) fn definition_digest(text: &str) -> Result<String, ApiError> {
    let mut documents = serde_yaml::Deserializer::from_str(text);
    let first = documents.next().ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "workflow definition is empty",
        )
    })?;
    let UniqueYaml(snapshot) =
        <UniqueYaml as serde::Deserialize>::deserialize(first).map_err(|e| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                format!("invalid workflow definition: {e}"),
            )
        })?;
    if documents.next().is_some() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "workflow definition must contain one YAML document",
        ));
    }
    crate::rule_api::saved_definition_digest(&snapshot)
}

struct UniqueYaml(Value);

impl<'de> serde::Deserialize<'de> for UniqueYaml {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueYamlVisitor)
    }
}

struct UniqueYamlVisitor;

impl<'de> serde::de::Visitor<'de> for UniqueYamlVisitor {
    type Value = UniqueYaml;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JSON-compatible YAML without duplicate mapping keys")
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::Bool(value)))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::Number(value.into())))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::Number(value.into())))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(|n| UniqueYaml(Value::Number(n)))
            .ok_or_else(|| E::custom("non-finite YAML number is not JSON"))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::String(value.to_owned())))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::String(value)))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::Null))
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(UniqueYaml(Value::Null))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        <UniqueYaml as serde::Deserialize>::deserialize(d)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(UniqueYaml(v)) = seq.next_element()? {
            values.push(v);
        }
        Ok(UniqueYaml(Value::Array(values)))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate YAML key `{key}`"
                )));
            }
            let UniqueYaml(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueYaml(Value::Object(values)))
    }
}

async fn save_definition(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    authenticate_publisher_service(state, headers, host, settings).await?;
    with_matching_host(args, host, || save_definition_verified(&state.pool, args)).await
}

/// Apply a definition after the caller and request Host have been verified.
/// This store boundary is shared by the native handler and PostgreSQL contract tests.
pub async fn save_definition_verified(
    pool: &sqlx::PgPool,
    args: &Value,
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let wf = uuid(args, "wfDefId")?;
    let revision = field(args, "sourceRevision")?
        .as_i64()
        .filter(|n| *n >= 1)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "sourceRevision must be positive",
            )
        })?;
    let actor = text(args, "actor")?;
    let namespace = text(args, "namespace")?;
    let name = text(args, "name")?;
    for (field_name, value) in [("actor", actor), ("namespace", namespace), ("name", name)] {
        if value.chars().count() > 126 {
            return Err(error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                format!("{field_name} exceeds 126 characters"),
            ));
        }
    }
    let version = text(args, "version")?;
    if version.chars().count() > 20 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "saved definition version exceeds 20 characters",
        ));
    }
    let definition = text(args, "definition")?;
    let lifecycle = text(args, "lifecycleStatus")?;
    if !matches!(lifecycle, "DRAFT" | "PUBLISHED" | "DEPRECATED") {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "invalid lifecycleStatus",
        ));
    }
    let catalog_visible = field(args, "catalogVisible")?.as_bool().ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "catalogVisible must be boolean",
        )
    })?;
    let active = field(args, "active")?.as_bool().ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "active must be boolean",
        )
    })?;
    definition_digest(definition)?;
    let owner_user: Option<Uuid> = match args.get("owner").and_then(|o| o.get("userId")) {
        Some(v) => Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "owner.userId must be a UUID",
            )
        })?),
        None => None,
    };
    let owner_position: Option<&str> = match args.get("owner").and_then(|o| o.get("positionId")) {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.trim().is_empty() && value.chars().count() <= 128)
                .ok_or_else(|| {
                    error(
                        StatusCode::BAD_REQUEST,
                        workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                        "owner.positionId must be a nonempty string of at most 128 characters",
                    )
                })?,
        ),
        None => None,
    };
    let mut tx = pool.begin().await.map_err(database_error)?;
    let existing = load_definition_head(&mut tx, host, wf).await?;
    let inserted = if existing.is_none() {
        // Concurrent first insertion: the unique-key loser then locks and reads
        // the winner before making the revision decision.
        sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,catalog_visible,owner_user_id,owner_position_id,aggregate_version,active,update_ts,update_user,source_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,CURRENT_TIMESTAMP,$13,$14) ON CONFLICT(host_id,wf_def_id) DO NOTHING")
            .bind(host).bind(wf).bind(namespace).bind(name).bind(version).bind(definition).bind(lifecycle)
            .bind(catalog_visible).bind(owner_user).bind(owner_position).bind(revision).bind(active).bind(actor).bind(revision)
            .execute(&mut *tx).await.map_err(database_error)?.rows_affected()==1
    } else {
        false
    };
    let stored = load_definition_head_for_update(&mut tx, host, wf)
        .await?
        .ok_or_else(|| database_error(sqlx::Error::RowNotFound))?;
    let (applied, stored_digest) = (
        stored.source_revision,
        definition_digest(&stored.definition)?,
    );
    let same = stored.namespace == namespace
        && stored.name == name
        && stored.version == version
        && stored.definition == definition
        && stored.lifecycle_status == lifecycle
        && stored.active == active
        && stored.catalog_visible.unwrap_or(false) == catalog_visible
        && stored.owner_user_id == owner_user
        && stored.owner_position_id.as_deref() == owner_position;
    let result = if revision < applied {
        "stale"
    } else if revision == applied {
        if inserted {
            "saved"
        } else {
            if !same {
                tx.rollback().await.map_err(database_error)?;
                return Err(conflict_with_details(
                    "equal source revision conflicts with stored definition state",
                    json!({"appliedRevision":applied,"definitionDigest":stored_digest}),
                ));
            }
            "unchanged"
        }
    } else {
        sqlx::query("UPDATE wf_definition_t SET namespace=$3,name=$4,version=$5,definition=$6,lifecycle_status=$7,catalog_visible=$8,owner_user_id=$9,owner_position_id=$10,aggregate_version=$11,active=$12,update_ts=CURRENT_TIMESTAMP,update_user=$13,source_revision=$14 WHERE host_id=$1 AND wf_def_id=$2")
                .bind(host).bind(wf).bind(namespace).bind(name).bind(version).bind(definition).bind(lifecycle)
                .bind(catalog_visible).bind(owner_user).bind(owner_position).bind(revision).bind(active).bind(actor).bind(revision)
                .execute(&mut *tx).await.map_err(database_error)?;
        if stored.owner_user_id != owner_user
            || stored.owner_position_id.as_deref() != owner_position
        {
            sqlx::query("UPDATE workflow_tool_binding_t SET owner_user_id=$3,owner_position_id=$4 WHERE host_id=$1 AND wf_def_id=$2 AND revision_status='pendingApproval'")
                    .bind(host).bind(wf).bind(owner_user).bind(owner_position).execute(&mut *tx).await.map_err(database_error)?;
        }
        "saved"
    };
    let stored = load_definition_head(&mut tx, host, wf)
        .await?
        .ok_or_else(|| database_error(sqlx::Error::RowNotFound))?;
    let receipt = json!({"result":result,"wfDefId":wf,"appliedRevision":stored.source_revision,"definitionDigest":definition_digest(&stored.definition)?});
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

#[derive(sqlx::FromRow)]
struct DefinitionHead {
    source_revision: i64,
    definition: String,
    namespace: String,
    name: String,
    version: String,
    lifecycle_status: String,
    catalog_visible: Option<bool>,
    active: bool,
    owner_user_id: Option<Uuid>,
    owner_position_id: Option<String>,
}

async fn load_definition_head(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    wf: Uuid,
) -> Result<Option<DefinitionHead>, ApiError> {
    sqlx::query_as("SELECT source_revision,definition,namespace,name,version,lifecycle_status,catalog_visible,active,owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2")
        .bind(host).bind(wf).fetch_optional(&mut **tx).await.map_err(database_error)
}
async fn load_definition_head_for_update(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    wf: Uuid,
) -> Result<Option<DefinitionHead>, ApiError> {
    sqlx::query_as("SELECT source_revision,definition,namespace,name,version,lifecycle_status,catalog_visible,active,owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2 FOR UPDATE")
        .bind(host).bind(wf).fetch_optional(&mut **tx).await.map_err(database_error)
}

#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Grant {
    grant_id: Uuid,
    tool_id: Uuid,
    tool_version: String,
    lightapi_digest: String,
    allowed_environments: Vec<String>,
}

async fn sync_grants(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    authenticate_publisher_service(state, headers, host, settings).await?;
    with_matching_host(args, host, || sync_grants_verified(&state.pool, args)).await
}

/// Apply a complete grant set after publisher and Host verification.
pub async fn sync_grants_verified(pool: &sqlx::PgPool, args: &Value) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let wf = uuid(args, "wfDefId")?;
    let revision = field(args, "sourceRevision")?
        .as_i64()
        .filter(|n| *n >= 1)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "sourceRevision must be positive",
            )
        })?;
    let actor = text(args, "actor")?;
    if actor.chars().count() > 126 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "actor exceeds 126 characters",
        ));
    }
    let mut grants: Vec<Grant> =
        serde_json::from_value(field(args, "grants")?.clone()).map_err(|e| {
            error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                e.to_string(),
            )
        })?;
    grants.sort_by_key(|g| g.grant_id);
    if grants.windows(2).any(|w| w[0].grant_id == w[1].grant_id) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "duplicate grantId",
        ));
    }
    if grants.len() > 1000 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "grant set exceeds 1000 entries",
        ));
    }
    let mut tool_ids = std::collections::HashSet::new();
    for grant in &grants {
        let mut environments = std::collections::HashSet::new();
        if !tool_ids.insert(grant.tool_id)
            || grant.tool_version.trim().is_empty()
            || grant.tool_version.chars().count() > 20
            || grant.allowed_environments.is_empty()
            || grant.allowed_environments.iter().any(|environment| {
                environment.trim().is_empty() || !environments.insert(environment)
            })
            || !grant.lightapi_digest.starts_with("sha256:")
            || grant.lightapi_digest.len() != 71
            || !grant.lightapi_digest[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "grant fields or set keys are invalid",
            ));
        }
    }
    let grant_value =
        serde_json::to_value(&grants).map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let requested_digest = digest_value(&grant_value)?;
    let empty_digest = digest_value(&json!([]))?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Definition FK and host scoping are resolved only after both token paths
    // and the body host have been checked.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2)",
    )
    .bind(host)
    .bind(wf)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    if !exists {
        return Err(error(
            StatusCode::NOT_FOUND,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "definition is not saved in Workflow",
        ));
    }
    sqlx::query("INSERT INTO workflow_definition_grant_sync_t(host_id,wf_def_id,source_revision,grant_set_digest) VALUES($1,$2,0,$3) ON CONFLICT(host_id,wf_def_id) DO NOTHING").bind(host).bind(wf).bind(&empty_digest).execute(&mut *tx).await.map_err(database_error)?;
    let current: (i64,String)=sqlx::query_as("SELECT source_revision,grant_set_digest FROM workflow_definition_grant_sync_t WHERE host_id=$1 AND wf_def_id=$2 FOR UPDATE").bind(host).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let (applied, stored_digest) = current;
    if revision < applied {
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT grant_id FROM workflow_tool_grant_t WHERE host_id=$1 AND wf_def_id=$2 AND active ORDER BY grant_id").bind(host).bind(wf).fetch_all(&mut *tx).await.map_err(database_error)?;
        let receipt = json!({"result":"stale","wfDefId":wf,"appliedRevision":applied,"grantSetDigest":stored_digest,"activeGrantIds":ids});
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if revision == applied {
        if requested_digest != stored_digest {
            tx.rollback().await.map_err(database_error)?;
            return Err(conflict_with_details(
                "equal source revision conflicts with stored grant set",
                json!({"appliedRevision":applied,"grantSetDigest":stored_digest}),
            ));
        }
        let ids:Vec<Uuid>=sqlx::query_scalar("SELECT grant_id FROM workflow_tool_grant_t WHERE host_id=$1 AND wf_def_id=$2 AND active ORDER BY grant_id").bind(host).bind(wf).fetch_all(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        return Ok(
            json!({"result":"unchanged","wfDefId":wf,"appliedRevision":applied,"grantSetDigest":stored_digest,"activeGrantIds":ids}),
        );
    }
    let ids: Vec<Uuid> = grants.iter().map(|g| g.grant_id).collect();
    sqlx::query("UPDATE workflow_tool_grant_t SET active=false,update_user=$3,update_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND wf_def_id=$2 AND active AND NOT (grant_id=ANY($4))").bind(host).bind(wf).bind(actor).bind(&ids).execute(&mut *tx).await.map_err(database_error)?;
    for g in &grants {
        // The conflict predicate is evaluated after PostgreSQL locks the
        // conflicting row, including a row inserted by a concurrent sync.
        let written = sqlx::query("INSERT INTO workflow_tool_grant_t(host_id,grant_id,tool_id,wf_def_id,tool_version,lightapi_digest,allowed_environments,aggregate_version,active,update_user,update_ts) VALUES($1,$2,$3,$4,$5,$6,$7,1,true,$8,CURRENT_TIMESTAMP) ON CONFLICT(host_id,grant_id) DO UPDATE SET tool_id=EXCLUDED.tool_id,tool_version=EXCLUDED.tool_version,lightapi_digest=EXCLUDED.lightapi_digest,allowed_environments=EXCLUDED.allowed_environments,aggregate_version=workflow_tool_grant_t.aggregate_version+1,active=true,update_user=EXCLUDED.update_user,update_ts=CURRENT_TIMESTAMP WHERE workflow_tool_grant_t.wf_def_id=EXCLUDED.wf_def_id")
            .bind(host).bind(g.grant_id).bind(g.tool_id).bind(wf).bind(&g.tool_version).bind(&g.lightapi_digest).bind(&g.allowed_environments).bind(actor)
            .execute(&mut *tx).await.map_err(database_error)?;
        if written.rows_affected() != 1 {
            return Err(error(
                StatusCode::CONFLICT,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "grantId belongs to another workflow definition",
            ));
        }
    }
    sqlx::query("UPDATE workflow_definition_grant_sync_t SET source_revision=$3,grant_set_digest=$4,synced_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND wf_def_id=$2").bind(host).bind(wf).bind(revision).bind(&requested_digest).execute(&mut *tx).await.map_err(database_error)?;
    let stored:(i64,String)=sqlx::query_as("SELECT source_revision,grant_set_digest FROM workflow_definition_grant_sync_t WHERE host_id=$1 AND wf_def_id=$2").bind(host).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let active_ids:Vec<Uuid>=sqlx::query_scalar("SELECT grant_id FROM workflow_tool_grant_t WHERE host_id=$1 AND wf_def_id=$2 AND active ORDER BY grant_id").bind(host).bind(wf).fetch_all(&mut *tx).await.map_err(database_error)?;
    let receipt = json!({"result":"synced","wfDefId":wf,"appliedRevision":stored.0,"grantSetDigest":stored.1,"activeGrantIds":active_ids});
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

async fn operation_begin(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    operation: Uuid,
    tool: &str,
    args: &Value,
) -> Result<Option<Value>, ApiError> {
    let digest = digest_value(&json!({"toolName":tool,"request":args}))?;
    sqlx::query("INSERT INTO workflow_publication_operation_t(host_id,operation_id,tool_name,request_digest,receipt) VALUES($1,$2,$3,$4,'{\"_pending\":true}'::jsonb) ON CONFLICT(host_id,operation_id) DO NOTHING")
        .bind(host).bind(operation).bind(tool).bind(&digest).execute(&mut **tx).await.map_err(database_error)?;
    let (old_tool,old_digest,receipt):(String,String,Value)=sqlx::query_as("SELECT tool_name,request_digest,receipt FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2 FOR UPDATE")
        .bind(host).bind(operation).fetch_one(&mut **tx).await.map_err(database_error)?;
    if old_tool != tool || old_digest != digest {
        return Err(error(
            StatusCode::CONFLICT,
            workflow_invocation_contract::ErrorCode::WorkflowIdempotencyConflict,
            "operationId was used with a different request",
        ));
    }
    if let Some(rejection) = receipt.get("_requestRejection") {
        // Version-1 rows created before the extension omitted the code; their
        // documented business code is WORKFLOW_INPUT_INVALID.
        let stored_code = rejection.get("code").and_then(Value::as_str)
            .unwrap_or("WORKFLOW_INPUT_INVALID");
        if stored_code != "WORKFLOW_INPUT_INVALID" {
            return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "unsupported stored rejection code"));
        }
        let message = rejection.get("message").and_then(Value::as_str)
            .unwrap_or("expectedAggregateVersion must be nonnegative");
        // Older rows have no separate evidence member. Preserve their original
        // version-1 marker while replaying newer rows exactly as committed.
        let evidence = rejection.get("evidence").cloned().unwrap_or_else(|| json!({
            "version": 1, "discriminator": "negativeExpectedAggregateVersion",
            "operationId": operation, "toolName": tool
        }));
        return Err(ApiError::input_invalid(message).with_details(json!({
            "requestValidation": evidence
        })));
    }
    if receipt.get("_pending").is_some() {
        Ok(None)
    } else {
        Ok(Some(receipt))
    }
}
async fn operation_finish(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    operation: Uuid,
    receipt: &Value,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE workflow_publication_operation_t SET receipt=$3 WHERE host_id=$1 AND operation_id=$2").bind(host).bind(operation).bind(receipt).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

async fn publish_definition(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
    cel_validator: &(dyn Fn(&str) -> Result<(), ApiError> + Send + Sync),
) -> Result<Value, ApiError> {
    let identity = user_and_publisher(state, headers, args, settings).await?;
    let positions = verified_positions(&identity.caller_claims);
    let actor = verified_user_id(&identity)?.to_owned();
    with_matching_host(args, identity.host_id, || {
        publish_definition_verified(&state.pool, args, &actor, &positions, cel_validator)
    })
    .await
}

/// Publish after the user, publisher assertion, Gateway and Host are verified.
pub async fn publish_definition_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
    cel_validator: &(dyn Fn(&str) -> Result<(), ApiError> + Send + Sync),
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let wf = uuid(args, "wfDefId")?;
    let operation = uuid(args, "operationId")?;
    let version = text(args, "version")?;
    let definition_text = text(args, "definition")?;
    let requested_digest = definition_digest(definition_text)?;
    if text(args, "expectedDefinitionDigest")? != requested_digest {
        return Err(ApiError::definition_mismatch(
            "expectedDefinitionDigest does not match definition",
        ));
    }
    let parsed: Value = serde_yaml::from_str(definition_text)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let definition: workflow_core::models::workflow::WorkflowDefinition =
        serde_yaml::from_str(definition_text)
            .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    crate::runtime_definition::validate_runtime_definition(
        &definition,
        crate::configuration::DEFAULT_MAXIMUM_PARALLELISM,
    )
    .map_err(ApiError::definition_mismatch)?;
    cel_validator(definition_text)?;
    let input_schema = parsed
        .pointer("/input/schema/document")
        .cloned()
        .unwrap_or(Value::Null);
    let schema_digest = digest_value(&input_schema)?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    if let Some(receipt) = operation_begin(
        &mut tx,
        host,
        operation,
        "workflow_definition_publish",
        args,
    )
    .await?
    {
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    let head:Option<(Option<Uuid>,Option<String>)>=sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2 FOR SHARE").bind(host).bind(wf).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (owner_user, owner_position) = head.ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
            "definition is not saved in Workflow",
        )
    })?;
    let exists:Option<(String,String,String,String)>=sqlx::query_as("SELECT definition_digest,version_status,schema_digest,binding_approval FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR UPDATE").bind(host).bind(wf).bind(version).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let requested_approval = match args.get("bindingApproval").and_then(Value::as_str) {
        None => "carryOver",
        Some("carryOver") => "carryOver",
        Some("reapprove") => "reapprove",
        _ => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
                "invalid bindingApproval",
            ));
        }
    };
    let is_owner = is_definition_owner(actor, positions, owner_user, owner_position.as_deref());
    let binding_approval = if requested_approval == "reapprove" && is_owner {
        "reapprove"
    } else {
        "carryOver"
    };
    let result = match exists {
        Some((old, status, stored_schema, stored_approval)) if old == requested_digest => {
            json!({"result":"unchanged","status":status,"wfDefId":wf,"version":version,"definitionDigest":old,"schemaDigest":stored_schema,"bindingApproval":stored_approval})
        }
        Some(_) => {
            return Err(ApiError::definition_mismatch(
                "definition version already exists with a different digest",
            ));
        }
        None => {
            // A concurrent first publisher may have inserted this version while
            // we waited for the head's share lock. Resolve the unique-key race
            // by reading the winner, then apply the same digest state machine.
            let inserted = sqlx::query("INSERT INTO wf_definition_version_t(host_id,wf_def_id,version,definition,definition_digest,schema_digest,binding_approval,version_status,published_by) VALUES($1,$2,$3,$4,$5,$6,$7,'active',$8) ON CONFLICT(host_id,wf_def_id,version) DO NOTHING")
                .bind(host).bind(wf).bind(version).bind(definition_text).bind(&requested_digest).bind(&schema_digest).bind(binding_approval).bind(actor)
                .execute(&mut *tx).await.map_err(database_error)?.rows_affected() == 1;
            if inserted {
                json!({"result":"published","status":"active","wfDefId":wf,"version":version,"definitionDigest":requested_digest,"schemaDigest":schema_digest,"bindingApproval":binding_approval})
            } else {
                let (stored_digest, status, stored_schema, stored_approval): (String, String, String, String) = sqlx::query_as("SELECT definition_digest,version_status,schema_digest,binding_approval FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR UPDATE")
                    .bind(host).bind(wf).bind(version).fetch_one(&mut *tx).await.map_err(database_error)?;
                if stored_digest != requested_digest {
                    return Err(ApiError::definition_mismatch(
                        "definition version already exists with a different digest",
                    ));
                }
                json!({"result":"unchanged","status":status,"wfDefId":wf,"version":version,"definitionDigest":stored_digest,"schemaDigest":stored_schema,"bindingApproval":stored_approval})
            }
        }
    };
    operation_finish(&mut tx, host, operation, &result).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(result)
}

async fn retire_definition(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<Value, ApiError> {
    let identity = user_and_publisher(state, headers, args, settings).await?;
    let actor = verified_user_id(&identity)?.to_owned();
    with_matching_host(args, identity.host_id, || {
        retire_definition_verified(&state.pool, args, &actor)
    })
    .await
}

/// Retire after the user, publisher assertion, Gateway and Host are verified.
pub async fn retire_definition_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let wf = uuid(args, "wfDefId")?;
    let operation = uuid(args, "operationId")?;
    let version = text(args, "version")?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    if let Some(receipt) =
        operation_begin(&mut tx, host, operation, "workflow_definition_retire", args).await?
    {
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    let row:Option<(String,Option<chrono::DateTime<chrono::Utc>>)>=sqlx::query_as("SELECT version_status,retired_ts FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR UPDATE").bind(host).bind(wf).bind(version).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (status, _) =
        row.ok_or_else(|| ApiError::definition_mismatch("definition version is unavailable"))?;
    if status == "retired" {
        let receipt =
            json!({"result":"unchanged","wfDefId":wf,"version":version,"withdrawnBindingIds":[]});
        operation_finish(&mut tx, host, operation, &receipt).await?;
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    let pins:Vec<String>=sqlx::query_scalar("SELECT DISTINCT tool_name FROM workflow_tool_binding_t WHERE host_id=$1 AND wf_def_id=$2 AND workflow_version=$3 AND active AND revision_status='approved' ORDER BY tool_name").bind(host).bind(wf).bind(version).fetch_all(&mut *tx).await.map_err(database_error)?;
    if !pins.is_empty() {
        return Err(error(
            StatusCode::CONFLICT,
            workflow_invocation_contract::ErrorCode::WorkflowStartRejected,
            format!("definition version is pinned by Tools: {}", pins.join(", ")),
        ));
    }
    // Publication heads are locked in tool order before their pending
    // revisions. An active pointer can refer to a different version and is
    // intentionally left alone.
    let heads: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT p.tool_id,p.pending_binding_id FROM workflow_tool_publication_t p JOIN workflow_tool_binding_t b ON b.host_id=p.host_id AND b.binding_id=p.pending_binding_id WHERE p.host_id=$1 AND b.wf_def_id=$2 AND b.workflow_version=$3 AND b.revision_status='pendingApproval' ORDER BY p.tool_id FOR UPDATE OF p")
        .bind(host).bind(wf).bind(version).fetch_all(&mut *tx).await.map_err(database_error)?;
    for (tool, binding) in heads {
        sqlx::query("UPDATE workflow_tool_publication_t SET pending_binding_id=NULL,aggregate_version=aggregate_version+1,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND tool_id=$2 AND pending_binding_id=$3")
            .bind(host).bind(tool).bind(binding).execute(&mut *tx).await.map_err(database_error)?;
    }
    let pending:Vec<(Uuid,Uuid,String)>=sqlx::query_as("SELECT binding_id,tool_id,approval_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND wf_def_id=$2 AND workflow_version=$3 AND revision_status='pendingApproval' FOR UPDATE").bind(host).bind(wf).bind(version).fetch_all(&mut *tx).await.map_err(database_error)?;
    let mut ids = Vec::new();
    for (binding, tool, digest) in pending {
        sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='withdrawn',active=false WHERE host_id=$1 AND binding_id=$2").bind(host).bind(binding).execute(&mut *tx).await.map_err(database_error)?;
        sqlx::query("INSERT INTO workflow_tool_binding_decision_t(host_id,decision_id,tool_id,binding_id,action,actor,approval_digest,operation_id) VALUES($1,$2,$3,$4,'withdraw',$5,$6,$7)").bind(host).bind(Uuid::new_v4()).bind(tool).bind(binding).bind(actor).bind(digest).bind(operation).execute(&mut *tx).await.map_err(database_error)?;
        ids.push(binding);
    }
    sqlx::query("UPDATE wf_definition_version_t SET version_status='retired',retired_by=$4,retired_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND wf_def_id=$2 AND version=$3").bind(host).bind(wf).bind(version).bind(actor).execute(&mut *tx).await.map_err(database_error)?;
    let receipt =
        json!({"result":"retired","wfDefId":wf,"version":version,"withdrawnBindingIds":ids});
    operation_finish(&mut tx, host, operation, &receipt).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_user_and_position_owner_paths() {
        let user = Uuid::new_v4();
        let positions = verified_positions(&json!({"pos":"team-a,team-b"}));
        assert!(is_definition_owner(
            &user.to_string(),
            &positions,
            Some(user),
            None
        ));
        assert!(is_definition_owner(
            "another-user",
            &positions,
            None,
            Some("team-b")
        ));
        assert!(!is_definition_owner(
            "another-user",
            &[],
            None,
            Some("team-b")
        ));
        assert!(!is_definition_owner(
            "another-user",
            &positions,
            Some(user),
            Some("team-c")
        ));
    }
    #[tokio::test]
    async fn signed_owner_claims_use_user_id_and_pos() {
        let security =
            light_security::SecurityRuntime::with_test_hs256_key("step03", TEST_KEY).await;
        let user = Uuid::new_v4();
        let token = signed(json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "token_use":"user","client_id":"portal-ui","user_id":user,"pos":"ops,reviewers",
            "host":Uuid::new_v4()}));
        let principal = verify_jwt_token(&security, &token, JwtExpiryMode::Enforce)
            .await
            .unwrap();
        let positions = verified_positions(&principal.claims);
        assert!(is_definition_owner(
            principal.user_id.as_deref().unwrap(),
            &positions,
            Some(user),
            None
        ));
        assert!(is_definition_owner(
            principal.user_id.as_deref().unwrap(),
            &positions,
            None,
            Some("reviewers")
        ));
        let without_pos = signed(json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "token_use":"user","client_id":"portal-ui","user_id":user,"host":Uuid::new_v4()}));
        let principal = verify_jwt_token(&security, &without_pos, JwtExpiryMode::Enforce)
            .await
            .unwrap();
        assert!(is_definition_owner(
            principal.user_id.as_deref().unwrap(),
            &verified_positions(&principal.claims),
            Some(user),
            None
        ));
        assert!(!is_definition_owner(
            principal.user_id.as_deref().unwrap(),
            &verified_positions(&principal.claims),
            None,
            Some("reviewers")
        ));
    }
    use axum::response::IntoResponse;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::path::PathBuf;
    const TEST_KEY: &[u8] = b"step03-publisher-signing-key-32-bytes";
    fn signed(claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("step03".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(TEST_KEY),
        )
        .unwrap()
    }
    fn publisher_claims(host: Uuid, purpose: &str) -> Value {
        json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "token_use":purpose,"client_id":"publisher-a","host":host})
    }
    fn scope_claims(host: Uuid, sid: &str) -> Value {
        json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "sid":sid,"host":host,"env":"dev"})
    }
    async fn test_service(
        security: &light_security::SecurityRuntime,
        allow: &[String],
        callers: &[String],
        headers: &HeaderMap,
        host: Uuid,
    ) -> Result<AuthPrincipal, ApiError> {
        authenticate_publisher_service_with(
            security,
            allow,
            callers,
            &[],
            "dev",
            headers,
            host,
            None,
        )
        .await
    }
    #[tokio::test]
    async fn signed_tokens_cover_both_publisher_entry_paths_before_store() {
        let security =
            light_security::SecurityRuntime::with_test_hs256_key("step03", TEST_KEY).await;
        let host = Uuid::new_v4();
        let other = Uuid::new_v4();
        let allow = vec!["publisher-a".to_owned()];
        let callers = vec!["gateway-a".to_owned()];
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-publisher-token",
            signed(publisher_claims(host, "app")).parse().unwrap(),
        );
        let direct = verify_with_purpose(
            &security,
            headers.get("x-publisher-token").unwrap().to_str().unwrap(),
            TokenUse::App,
            &[],
        )
        .await;
        assert!(direct.is_ok(), "{:?}", direct.err());
        assert!(
            verify_publisher_header_with(&security, &allow, &headers, host, None)
                .await
                .is_ok(),
            "{:?}",
            verify_publisher_header_with(&security, &allow, &headers, host, None)
                .await
                .err()
        );
        assert!(
            verify_publisher_header_with(&security, &allow, &headers, other, None)
                .await
                .is_err()
        );
        assert!(
            verify_publisher_header_with(&security, &[], &headers, host, None)
                .await
                .is_err()
        );
        headers.remove("x-publisher-token");
        assert!(
            verify_publisher_header_with(&security, &allow, &headers, host, None)
                .await
                .is_err()
        );
        headers.insert(
            "x-publisher-token",
            signed(publisher_claims(host, "user")).parse().unwrap(),
        );
        assert!(
            verify_publisher_header_with(&security, &allow, &headers, host, None)
                .await
                .is_err()
        );

        headers.insert(
            "authorization",
            format!("Bearer {}", signed(publisher_claims(host, "app")))
                .parse()
                .unwrap(),
        );
        headers.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope_claims(host, "gateway-a")))
                .parse()
                .unwrap(),
        );
        assert!(
            test_service(&security, &allow, &callers, &headers, host)
                .await
                .is_ok()
        );
        assert!(
            test_service(&security, &allow, &callers, &headers, other)
                .await
                .is_err()
        );
        headers.remove("authorization");
        assert!(
            test_service(&security, &allow, &callers, &headers, host)
                .await
                .is_err()
        );
        headers.insert(
            "authorization",
            format!("Bearer {}", signed(publisher_claims(host, "user")))
                .parse()
                .unwrap(),
        );
        assert!(
            test_service(&security, &allow, &callers, &headers, host)
                .await
                .is_err()
        );
        headers.insert(
            "authorization",
            format!("Bearer {}", signed(publisher_claims(host, "app")))
                .parse()
                .unwrap(),
        );
        headers.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope_claims(host, "wrong-gateway")))
                .parse()
                .unwrap(),
        );
        assert!(
            test_service(&security, &allow, &callers, &headers, host)
                .await
                .is_err()
        );
        headers.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope_claims(host, "gateway-a")))
                .parse()
                .unwrap(),
        );
        assert!(
            test_service(&security, &allow, &callers, &headers, host)
                .await
                .is_ok()
        );
        let scoped_calls = std::sync::atomic::AtomicUsize::new(0);
        let rejected: Result<(), ApiError> =
            with_matching_host(&json!({"hostId":other}), host, || async {
                scoped_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(rejected.is_err());
        assert_eq!(
            scoped_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "neither scoped storage nor receipt replay can be reached"
        );
    }

    #[tokio::test]
    async fn handler_authentication_paths_reject_before_lazy_store_or_receipt() {
        let host = Uuid::new_v4();
        let other = Uuid::new_v4();
        let state = RuleApiState::for_publication_test(
            light_security::SecurityRuntime::with_test_hs256_key("step03", TEST_KEY).await,
            host,
        );
        let user = signed(json!({"iss":"step03","aud":"workflow","exp":4102444800u64,
            "token_use":"user","client_id":"publisher-a","uid":Uuid::new_v4(),"host":host}));
        let app = signed(publisher_claims(host, "app"));
        let scope = signed(scope_claims(host, "gateway-a"));
        let mut user_headers = HeaderMap::new();
        user_headers.insert("authorization", format!("Bearer {user}").parse().unwrap());
        user_headers.insert("x-scope-token", format!("Bearer {scope}").parse().unwrap());
        user_headers.insert("x-publisher-token", app.parse().unwrap());
        assert!(
            user_and_publisher(&state, &user_headers, &json!({"hostId":host}), None)
                .await
                .is_ok()
        );
        let rejected = dispatch("workflow_definition_retire", &state, &user_headers,
            &json!({"hostId":other,"wfDefId":Uuid::new_v4(),"version":"1.0.0","operationId":Uuid::new_v4()}),
            None, &|_| Ok(())).await.unwrap_err();
        assert_eq!(rejected.into_response().status(), StatusCode::FORBIDDEN);
        for name in [
            "workflow_binding_get",
            "workflow_binding_list",
            "workflow_binding_decide",
            "workflow_binding_revoke",
        ] {
            let rejected = dispatch(
                name,
                &state,
                &user_headers,
                &json!({"hostId":other,"bindingId":Uuid::new_v4(),"operationId":Uuid::new_v4()}),
                None,
                &|_| Ok(()),
            )
            .await
            .unwrap_err();
            assert_eq!(
                rejected.into_response().status(),
                StatusCode::FORBIDDEN,
                "{name} must deny before scoped storage or receipt lookup"
            );
        }
        let mut bad_gateway = user_headers.clone();
        bad_gateway.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope_claims(host, "wrong-gateway")))
                .parse()
                .unwrap(),
        );
        let rejected = dispatch(
            "workflow_binding_get",
            &state,
            &bad_gateway,
            &json!({"hostId":host,"bindingId":Uuid::new_v4()}),
            None,
            &|_| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.into_response().status(), StatusCode::UNAUTHORIZED);
        let mut app_as_user = user_headers.clone();
        app_as_user.insert(
            "authorization",
            format!(
                "Bearer {}",
                signed(json!({
            "iss":"step03","aud":"workflow","exp":4102444800u64,"token_use":"app",
            "client_id":"publisher-a","uid":Uuid::new_v4(),"host":host}))
            )
            .parse()
            .unwrap(),
        );
        let rejected = dispatch(
            "workflow_binding_get",
            &state,
            &app_as_user,
            &json!({"hostId":host,"bindingId":Uuid::new_v4()}),
            None,
            &|_| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.into_response().status(), StatusCode::FORBIDDEN);
        user_headers.remove("x-publisher-token");
        let authenticated =
            binding::user_identity(&state, &user_headers, &json!({"hostId":host})).await;
        assert!(
            authenticated.is_ok(),
            "the four user-only tools share this path without a publisher token"
        );
        assert!(
            user_and_publisher(&state, &user_headers, &json!({"hostId":host}), None)
                .await
                .is_err()
        );
        user_headers.insert("x-publisher-token", user.parse().unwrap());
        assert!(
            user_and_publisher(&state, &user_headers, &json!({"hostId":host}), None)
                .await
                .is_err()
        );
        let mut service_headers = HeaderMap::new();
        service_headers.insert(
            "authorization",
            format!("Bearer {}", signed(publisher_claims(host, "app")))
                .parse()
                .unwrap(),
        );
        service_headers.insert("x-scope-token", format!("Bearer {scope}").parse().unwrap());
        assert!(
            authenticate_publisher_service(&state, &service_headers, host, None)
                .await
                .is_ok()
        );
        let rejected = dispatch(
            "workflow_definition_save",
            &state,
            &service_headers,
            &json!({"hostId":other}),
            None,
            &|_| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.into_response().status(), StatusCode::FORBIDDEN);
        service_headers.remove("authorization");
        assert!(
            authenticate_publisher_service(&state, &service_headers, host, None)
                .await
                .is_err()
        );
        service_headers.insert("authorization", format!("Bearer {user}").parse().unwrap());
        assert!(
            authenticate_publisher_service(&state, &service_headers, host, None)
                .await
                .is_err()
        );
        service_headers.insert(
            "authorization",
            format!("Bearer {}", signed(publisher_claims(host, "app")))
                .parse()
                .unwrap(),
        );
        service_headers.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope_claims(host, "wrong-gateway")))
                .parse()
                .unwrap(),
        );
        assert!(
            authenticate_publisher_service(&state, &service_headers, host, None)
                .await
                .is_err()
        );
    }
    #[test]
    fn digest_fixture_semantics_preserve_order_and_types() {
        let reordered = definition_digest("a: 1\nb: [yes, null]\n").unwrap();
        let ordered = definition_digest("b: [yes, null]\na: 1\n").unwrap();
        assert_eq!(reordered, ordered);
        assert_ne!(
            reordered,
            definition_digest("a: 1\nb: [null, yes]\n").unwrap()
        );
        assert!(definition_digest("x: 1\nx: 2\n").is_err());
        assert!(definition_digest("a: 1\n---\nb: 2\n").is_err());
    }
    #[test]
    fn publisher_claims_allow_only_configured_host_client() {
        let principal = AuthPrincipal {
            client_id: Some("service-a".into()),
            host: Some("00000000-0000-4000-8000-000000000001".into()),
            ..Default::default()
        };
        assert_eq!(publisher_client(&principal), Some("service-a"));
        assert_eq!(
            host_claim(&principal),
            Some(Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap())
        );
    }
    #[test]
    fn bad_publisher_header_fails_before_store_dispatch() {
        let headers = HeaderMap::new();
        assert!(publisher_header_token(&headers).is_err());
    }

    #[test]
    fn wrong_publisher_client_and_host_are_denied() {
        let host = Uuid::new_v4();
        let other = Uuid::new_v4();
        let principal = AuthPrincipal {
            client_id: Some("portal-publisher".into()),
            host: Some(host.to_string()),
            ..Default::default()
        };
        assert!(assert_publisher_claims(&principal, host, &[]).is_err());
        assert!(assert_publisher_claims(&principal, host, &["another-client".into()]).is_err());
        assert!(assert_publisher_claims(&principal, other, &["portal-publisher".into()]).is_err());
        assert!(assert_publisher_claims(&principal, host, &["portal-publisher".into()]).is_ok());
        assert!(assert_body_host(&json!({"hostId":other}), host).is_err());
    }

    #[tokio::test]
    async fn host_mismatch_does_not_call_scoped_store() {
        let host = Uuid::new_v4();
        let other = Uuid::new_v4();
        let called = std::sync::atomic::AtomicBool::new(false);
        let result: Result<(), ApiError> =
            with_matching_host(&json!({"hostId":other}), host, || async {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn user_purpose_is_denied_even_with_allowlisted_client_id() {
        let host = Uuid::new_v4();
        let principal = AuthPrincipal {
            client_id: Some("portal-publisher".into()),
            host: Some(host.to_string()),
            ..Default::default()
        };
        assert!(assert_publisher_claims(&principal, host, &["portal-publisher".into()]).is_ok());
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"token_use":"user","exp":4102444800}"#);
        let token = format!("{header}.{payload}.signature");
        assert!(
            light_security::token_purpose::validate_verified_purpose(
                &token,
                &principal,
                TokenUse::App,
                &[],
            )
            .is_err(),
            "both sync and header publisher paths require App purpose"
        );
    }

    #[test]
    fn digest_fixtures_match_workflow_values_and_rejections() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/definition-digest");
        let expected: Value =
            serde_json::from_slice(&std::fs::read(root.join("expected.json")).unwrap()).unwrap();
        let names = [
            "key-order.yaml",
            "key-order-reversed.yaml",
            "whitespace-and-comments.yaml",
            "whitespace-normalized.yaml",
            "duplicate-keys.yaml",
            "yaml-11-words.yaml",
            "unquoted-timestamp.yaml",
            "numeric-types.yaml",
            "merge-keys.yaml",
            "explicit-null.yaml",
            "omitted-null.yaml",
        ];
        for name in names {
            let text = std::fs::read_to_string(root.join(name)).unwrap();
            match expected
                .get(name)
                .expect("every fixture has an expected entry")
            {
                Value::Null => {
                    assert!(definition_digest(&text).is_err(), "{name} must be rejected")
                }
                Value::String(want) => {
                    let digest = definition_digest(&text).unwrap();
                    assert_eq!(&digest, want, "{name}");
                    let parsed: Value = serde_yaml::from_str(&text).unwrap();
                    assert_eq!(
                        digest,
                        crate::rule_api::saved_definition_digest(&parsed).unwrap(),
                        "saved head digest parity for {name}"
                    );
                }
                other => panic!("invalid expected result for {name}: {other}"),
            }
        }
        assert_eq!(names.len(), expected.as_object().unwrap().len());
    }

    #[test]
    fn definition_schema_digest_matches_shared_portal_fixtures() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/definition-digest");
        let expected: Value =
            serde_json::from_slice(&std::fs::read(root.join("schema-expected.json")).unwrap())
                .unwrap();
        for (name, want) in expected.as_object().unwrap() {
            let text = std::fs::read_to_string(root.join(name)).unwrap();
            let parsed: Value = serde_yaml::from_str(&text).unwrap();
            let schema = parsed
                .pointer("/input/schema/document")
                .cloned()
                .unwrap_or(Value::Null);
            assert_eq!(
                digest_value(&schema).unwrap(),
                want.as_str().unwrap(),
                "{name}"
            );
        }
    }
}
