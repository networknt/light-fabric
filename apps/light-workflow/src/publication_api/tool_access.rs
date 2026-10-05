use super::*;
fn invalid(message: &str) -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        workflow_invocation_contract::ErrorCode::WorkflowInputInvalid,
        message,
    )
}
fn admin_claim(claims: &Value) -> bool {
    ["roles", "role"]
        .iter()
        .filter_map(|name| claims.get(*name))
        .any(|value| match value {
            Value::Array(values) => values.iter().any(|v| v.as_str() == Some("genai-admin")),
            Value::String(value) => value.split([',', ' ']).any(|v| v == "genai-admin"),
            _ => false,
        })
}

pub(super) async fn publish(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let (identity, _) = crate::rule_api::authenticate(state, headers).await?;
    verified_user_id(&identity)?;
    if identity.host_id != host {
        return Err(ApiError::policy_denied("policy publisher Host mismatch"));
    }
    if !admin_claim(&identity.caller_claims) {
        return Err(ApiError::policy_denied("genai-admin publisher required"));
    }
    // Gateway verifies this dedicated operation's ACL; Portal additionally checks current membership.
    with_matching_host(args, host, || publish_verified(&state.pool, args)).await
}

/// Authenticated publication callers only. Full replacement including disable tombstones.
pub async fn publish_verified(pool: &sqlx::PgPool, args: &Value) -> Result<Value, ApiError> {
    let host = uuid(args, "hostId")?;
    let tool = uuid(args, "toolId")?;
    let policy = uuid(args, "policyId")?;
    let revision = field(args, "sourceRevision")?
        .as_i64()
        .filter(|r| *r > 0)
        .ok_or_else(|| invalid("invalid sourceRevision"))?;
    let capability = text(args, "capabilityRef")?;
    let version = text(args, "toolVersion")?;
    let digest = text(args, "lightapiDigest")?;
    let actor = text(args, "actor")?;
    let environments: Vec<String> =
        serde_json::from_value(field(args, "allowedEnvironments")?.clone())
            .map_err(|_| invalid("invalid environments"))?;
    let methods: Vec<String> = serde_json::from_value(field(args, "allowedMethods")?.clone())
        .map_err(|_| invalid("invalid methods"))?;
    let enabled = field(args, "enabled")?
        .as_bool()
        .ok_or_else(|| invalid("enabled required"))?;
    if version.len() > 20
        || actor.chars().count() > 126
        || digest.len() != 71
        || !digest.starts_with("sha256:")
        || !digest[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || environments.is_empty()
        || environments.len() > 16
        || environments.iter().any(|s| s.trim().is_empty())
        || methods.is_empty()
        || methods.len() > 6
        || methods
            .iter()
            .any(|m| !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&m.as_str()))
        || environments
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != environments.len()
        || methods
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != methods.len()
    {
        return Err(invalid("invalid reviewed policy fields"));
    }
    let publication_digest = digest_value(args)?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("host-tool-access:{host}:{tool}"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let current:Option<(i64,String,Uuid)>=sqlx::query_as("SELECT source_revision,publication_digest,policy_id FROM tool_workflow_access_t WHERE host_id=$1 AND tool_id=$2 FOR UPDATE")
        .bind(host).bind(tool).fetch_optional(&mut *tx).await.map_err(database_error)?;
    if let Some((applied, stored, id)) = current {
        if id != policy {
            return Err(ApiError::policy_denied("policy identity changed"));
        }
        if revision < applied {
            tx.commit().await.map_err(database_error)?;
            return Ok(
                json!({"result":"stale","appliedRevision":applied,"publicationDigest":stored}),
            );
        }
        if revision == applied {
            if publication_digest != stored {
                return Err(conflict_with_details(
                    "equal policy revision conflict",
                    json!({"appliedRevision":applied}),
                ));
            }
            tx.commit().await.map_err(database_error)?;
            return Ok(
                json!({"result":"unchanged","appliedRevision":applied,"publicationDigest":stored}),
            );
        }
    }
    sqlx::query("INSERT INTO tool_workflow_access_t(host_id,tool_id,policy_id,capability_ref,tool_version,lightapi_digest,
        allowed_environments,allowed_methods,enabled,source_revision,publication_digest,actor)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT(host_id,tool_id) DO UPDATE SET
        capability_ref=EXCLUDED.capability_ref,tool_version=EXCLUDED.tool_version,lightapi_digest=EXCLUDED.lightapi_digest,
        allowed_environments=EXCLUDED.allowed_environments,allowed_methods=EXCLUDED.allowed_methods,enabled=EXCLUDED.enabled,
        source_revision=EXCLUDED.source_revision,publication_digest=EXCLUDED.publication_digest,actor=EXCLUDED.actor,published_ts=now()")
        .bind(host).bind(tool).bind(policy).bind(capability).bind(version).bind(digest).bind(environments).bind(methods)
        .bind(enabled).bind(revision).bind(&publication_digest).bind(actor).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)?;
    Ok(
        json!({"result":"published","appliedRevision":revision,"publicationDigest":publication_digest}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    const KEY: &[u8] = b"isolated-host-access-signing-key-32bytes";
    fn signed(claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("host-access".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(KEY),
        )
        .unwrap()
    }
    fn headers(host: Uuid, admin: bool) -> HeaderMap {
        let user = signed(
            json!({"iss":"host-access","aud":"workflow","exp":4102444800u64,"token_use":"user",
            "sub":Uuid::new_v4(),"uid":Uuid::new_v4(),"client_id":"portal-ui","host":host,"roles":if admin{vec!["genai-admin"]}else{vec!["reader"]}}),
        );
        let scope = signed(
            json!({"iss":"host-access","aud":"workflow","exp":4102444800u64,"sid":"gateway-a","host":host,"env":"dev"}),
        );
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {user}").parse().unwrap());
        headers.insert("x-scope-token", format!("Bearer {scope}").parse().unwrap());
        headers
    }
    #[tokio::test]
    async fn non_admin_foreign_host_and_missing_gateway_are_denied_before_store() {
        let host = Uuid::new_v4();
        let state = RuleApiState::for_publication_test(
            light_security::SecurityRuntime::with_test_hs256_key("host-access", KEY).await,
            host,
        );
        let args = json!({"hostId":host});
        assert_eq!(
            publish(&state, &headers(host, false), &args)
                .await
                .unwrap_err()
                .into_response()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            publish(
                &state,
                &headers(host, true),
                &json!({"hostId":Uuid::new_v4()})
            )
            .await
            .unwrap_err()
            .into_response()
            .status(),
            StatusCode::FORBIDDEN
        );
        let mut no_gateway = headers(host, true);
        no_gateway.remove("x-scope-token");
        assert!(publish(&state, &no_gateway, &args).await.is_err());
    }
    #[tokio::test]
    #[ignore = "requires explicitly supplied disposable PostgreSQL database"]
    async fn authenticated_admin_publication_reaches_operational_store() {
        let host = Uuid::new_v4();
        let tool = Uuid::new_v4();
        let mut state = RuleApiState::for_publication_test(
            light_security::SecurityRuntime::with_test_hs256_key("host-access", KEY).await,
            host,
        );
        let url = std::env::var("DATABASE_URL").expect("disposable DATABASE_URL required");
        let fixture_url = reqwest::Url::parse(&url).unwrap();
        assert_eq!(fixture_url.host_str(), Some("127.0.0.1"));
        assert_eq!(fixture_url.path(), "/g03_fixture");
        assert_eq!(fixture_url.username(), "operations_workflow_runtime");
        state.pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path=workflow_ops")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let args = json!({"hostId":host,"toolId":tool,"policyId":tool,"capabilityRef":"GITHUB/getIssue",
            "toolVersion":"1.0.0","lightapiDigest":format!("sha256:{}","a".repeat(64)),"allowedEnvironments":["dev"],
            "allowedMethods":["GET"],"enabled":true,"sourceRevision":1,"actor":"isolated-admin"});
        assert_eq!(
            publish(&state, &headers(host, true), &args).await.unwrap()["appliedRevision"],
            1
        );
        assert_eq!(
            publish(&state, &headers(host, true), &args).await.unwrap()["result"],
            "unchanged"
        );
        let enabled: bool = sqlx::query_scalar(
            "SELECT enabled FROM tool_workflow_access_t WHERE host_id=$1 AND tool_id=$2",
        )
        .bind(host)
        .bind(tool)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert!(enabled);
    }
}
