//! Host/Tool authority is checked only during atomic acceptance, then pinned.
use crate::invocation::InvocationAcceptError;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

fn pins<'a>(
    node: &'a Value,
    out: &mut Vec<(&'a Value, String)>,
) -> Result<(), InvocationAcceptError> {
    if let Some(map) = node.as_object() {
        if let Some(pin) = map.get("metadata").and_then(|m| m.get("workflowTool")) {
            let method = map
                .get("with")
                .and_then(|w| w.get("method"))
                .and_then(Value::as_str)
                .ok_or(InvocationAcceptError::ToolAccessDenied)?
                .to_ascii_uppercase();
            out.push((pin, method));
        }
        for child in map.values() {
            pins(child, out)?;
        }
    } else if let Some(array) = node.as_array() {
        for child in array {
            pins(child, out)?;
        }
    }
    Ok(())
}
fn text<'a>(pin: &'a Value, key: &str) -> Result<&'a str, InvocationAcceptError> {
    pin.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(InvocationAcceptError::ToolAccessDenied)
}

pub async fn pin_accepted(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    wf: Uuid,
    process: Uuid,
    binding: Option<Uuid>,
    definition: &Value,
    environment: &str,
) -> Result<(), InvocationAcceptError> {
    let mut tools = Vec::new();
    pins(definition, &mut tools)?;
    sqlx::query("INSERT INTO workflow_tool_authority_acceptance_t(host_id,process_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(host).bind(process).execute(&mut **tx).await?;
    // Shared with publication; stable ordering prevents lock inversion with multi-Tool starts.
    tools.sort_by_key(|(pin, _)| {
        pin.get("toolId")
            .and_then(Value::as_str)
            .unwrap_or_default()
    });
    for (pin, method) in tools {
        let tool: Uuid = text(pin, "toolId")?
            .parse()
            .map_err(|_| InvocationAcceptError::ToolAccessDenied)?;
        let capability = text(pin, "capabilityRef")?;
        let version = text(pin, "version")?;
        let digest = text(pin, "lightapiDigest")?;
        if !pin
            .get("allowedEnvironments")
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.as_str() == Some(environment))
            })
        {
            return Err(InvocationAcceptError::ToolAccessDenied);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("host-tool-access:{host}:{tool}"))
            .execute(&mut **tx)
            .await?;
        // Broad permission may exist before any definition or binding exists.
        let gateway = std::env::var("LIGHT_GATEWAY_MCP_URL")
            .unwrap_or_else(|_| "https://light-gateway:8443/mcp".into());
        let gateway =
            reqwest::Url::parse(&gateway).map_err(|_| InvocationAcceptError::ToolAccessDenied)?;
        let gateway_origin = gateway.origin().ascii_serialization();
        let broad:Option<(Uuid,i64,String,Vec<String>)>=sqlx::query_as(
            "SELECT policy_id,source_revision,publication_digest,allowed_methods FROM tool_workflow_access_t
             WHERE host_id=$1 AND tool_id=$2 AND capability_ref=$3 AND tool_version=$4 AND lightapi_digest=$5
             AND enabled AND $6=ANY(allowed_environments) AND $7=ANY(allowed_methods)
             AND EXISTS (SELECT 1 FROM workflow_endpoint_target_t t JOIN workflow_tool_binding_t b
               ON b.host_id=t.host_id AND b.binding_id=t.binding_id
               WHERE t.host_id=$1 AND b.wf_def_id=$8 AND ($9::uuid IS NULL OR b.binding_id=$9)
               AND t.endpoint_ref=$3 AND $7=ANY(t.allowed_methods) AND b.active AND t.active
               AND t.resolution_document IS NOT NULL AND t.endpoint_uri IN ($10,$11)) FOR SHARE")
            .bind(host).bind(tool).bind(capability).bind(version).bind(digest).bind(environment).bind(&method)
            .bind(wf).bind(binding).bind(&gateway_origin).bind(format!("{gateway_origin}/"))
            .fetch_optional(&mut **tx).await?;
        let (source, authority, revision, authority_digest, methods) = if let Some((
            id,
            rev,
            digest,
            methods,
        )) = broad
        {
            ("HOST_TOOL", id, rev, digest, methods)
        } else {
            let specific:Option<(Uuid,i64,Vec<String>)>=sqlx::query_as(
                "SELECT grant_id,aggregate_version,allowed_environments FROM workflow_tool_grant_t
                 WHERE host_id=$1 AND wf_def_id=$2 AND tool_id=$3 AND tool_version=$4 AND lightapi_digest=$5
                 AND active AND $6=ANY(allowed_environments) FOR SHARE")
                .bind(host).bind(wf).bind(tool).bind(version).bind(digest).bind(environment)
                .fetch_optional(&mut **tx).await?;
            let (id, rev, environments) =
                specific.ok_or(InvocationAcceptError::ToolAccessDenied)?;
            let evidence = workflow_invocation_contract::canonical_sha256(
                &json!({"grantId":id,"revision":rev,
                "toolId":tool,"version":version,"digest":digest,"allowedEnvironments":environments}),
            )?;
            ("SPECIFIC_GRANT", id, rev, evidence, vec![method.clone()])
        };
        let target:Option<(Uuid,String,Option<Value>)>=sqlx::query_as(
            "SELECT b.binding_id,t.endpoint_uri,t.resolution_document FROM workflow_endpoint_target_t t JOIN workflow_tool_binding_t b
             ON b.host_id=t.host_id AND b.binding_id=t.binding_id
             WHERE t.host_id=$1 AND b.wf_def_id=$2 AND ($3::uuid IS NULL OR b.binding_id=$3) AND t.endpoint_ref=$4
             AND $5=ANY(t.allowed_methods) AND b.active AND t.active
             AND ($6<>'HOST_TOOL' OR (t.resolution_document IS NOT NULL AND t.endpoint_uri IN ($7,$8)))
             ORDER BY b.binding_id LIMIT 1 FOR SHARE OF b,t")
            .bind(host).bind(wf).bind(binding).bind(capability).bind(&method).bind(source)
            .bind(&gateway_origin).bind(format!("{gateway_origin}/"))
            .fetch_optional(&mut **tx).await?;
        let (target_binding, endpoint_uri, resolution_document) =
            target.ok_or(InvocationAcceptError::ToolAccessDenied)?;
        if source == "HOST_TOOL" {
            let gateway = std::env::var("LIGHT_GATEWAY_MCP_URL")
                .unwrap_or_else(|_| "https://light-gateway:8443/mcp".into());
            let gateway = reqwest::Url::parse(&gateway)
                .map_err(|_| InvocationAcceptError::ToolAccessDenied)?;
            let endpoint = reqwest::Url::parse(&endpoint_uri)
                .map_err(|_| InvocationAcceptError::ToolAccessDenied)?;
            if endpoint.origin() != gateway.origin()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || !["", "/"].contains(&endpoint.path())
            {
                return Err(InvocationAcceptError::ToolAccessDenied);
            }
        }
        sqlx::query("INSERT INTO workflow_accepted_tool_authority_t(host_id,process_id,tool_id,capability_ref,
             tool_version,lightapi_digest,environment,allowed_methods,authorization_source,authority_id,
             authority_revision,authority_digest,binding_id,endpoint_uri,resolution_document) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT(host_id,process_id,tool_id,environment) DO NOTHING")
            .bind(host).bind(process).bind(tool).bind(capability).bind(version).bind(digest).bind(environment)
            .bind(methods).bind(source).bind(authority).bind(revision).bind(authority_digest)
            .bind(target_binding).bind(endpoint_uri).bind(resolution_document).execute(&mut **tx).await?;
        let matching:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_accepted_tool_authority_t
            WHERE host_id=$1 AND process_id=$2 AND tool_id=$3 AND environment=$4 AND capability_ref=$5
            AND tool_version=$6 AND lightapi_digest=$7 AND $8=ANY(allowed_methods))")
            .bind(host).bind(process).bind(tool).bind(environment).bind(capability).bind(version).bind(digest).bind(method)
            .fetch_one(&mut **tx).await?;
        if !matching {
            return Err(InvocationAcceptError::ToolAccessDenied);
        }
    }
    Ok(())
}
