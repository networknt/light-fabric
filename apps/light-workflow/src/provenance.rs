use execution_runner_protocol::{NormalizedExecutionResult, canonical_sha256};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedBuildProvenance {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    pub predicate_type: String,
    pub predicate: SlsaPredicate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SlsaPredicate {
    pub build_definition: serde_json::Value,
    pub run_details: serde_json::Value,
}

pub fn generate_trusted_provenance(
    result: &NormalizedExecutionResult,
    input_digests: Vec<String>,
) -> Result<(TrustedBuildProvenance, String), execution_runner_protocol::CanonicalJsonError> {
    let subject = result
        .artifacts
        .iter()
        .map(|artifact| Subject {
            name: artifact.logical_name.clone(),
            digest: BTreeMap::from([(
                "sha256".into(),
                artifact
                    .digest
                    .strip_prefix("sha256:")
                    .unwrap_or(&artifact.digest)
                    .to_ascii_lowercase(),
            )]),
        })
        .collect();
    let statement = TrustedBuildProvenance {
        statement_type: STATEMENT_TYPE.into(),
        subject,
        predicate_type: PREDICATE_TYPE.into(),
        predicate: SlsaPredicate {
            build_definition: json!({
                "buildType": "https://lightapi.net/light-workflow/execution/v1",
                "externalParameters": {
                    "definitionDigest": result.definition_digest,
                    "policyDigest": result.policy_digest,
                    "commandTemplateDigest": result.command_template_digest,
                    "compatibilityDigest": result.compatibility_digest
                },
                "internalParameters": {},
                "resolvedDependencies": input_digests.into_iter().map(|digest| json!({
                    "uri": "urn:light-workflow:input",
                    "digest": {"sha256": digest.strip_prefix("sha256:").unwrap_or(&digest)}
                })).collect::<Vec<_>>()
            }),
            run_details: json!({
                "builder": {"id": "https://lightapi.net/builders/light-workflow"},
                "metadata": {
                    "invocationId": result.execution_id.to_string(),
                    "startedOn": result.started_at,
                    "finishedOn": result.finished_at
                },
                "byproducts": [{
                    "name": "backend-operation",
                    "content": result.backend_operation_id
                }]
            }),
        },
    };
    let digest = canonical_sha256(&statement)?;
    Ok((statement, digest))
}

pub async fn persist_trusted_provenance(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    result: &NormalizedExecutionResult,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let input_digests: Vec<String> = sqlx::query_scalar(
        "SELECT content_digest FROM execution_input_t
         WHERE host_id=$1 AND execution_id=$2 AND staging_state='VERIFIED'
         ORDER BY input_id",
    )
    .bind(host_id)
    .bind(result.execution_id.0)
    .fetch_all(&mut **tx)
    .await?;
    let (statement, digest) = generate_trusted_provenance(result, input_digests)?;
    let statement_value = serde_json::to_value(&statement)?;
    sqlx::query(
        "INSERT INTO execution_provenance_t(host_id,provenance_id,execution_id,statement,statement_digest,predicate_type,trusted_generator)
         VALUES($1,$2,$3,$4,$5,$6,'light-workflow')
         ON CONFLICT(host_id,execution_id,statement_digest) DO NOTHING",
    )
    .bind(host_id)
    .bind(Uuid::now_v7())
    .bind(result.execution_id.0)
    .bind(statement_value)
    .bind(&digest)
    .bind(&statement.predicate_type)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE workflow_artifact_t SET provenance_digest=$3,provenance_reference=$4,updated_ts=now() WHERE host_id=$1 AND execution_id=$2 AND promotion_state='BOUND' AND verification_state='VERIFIED'")
        .bind(host_id).bind(result.execution_id.0).bind(&digest)
        .bind(format!("provenance://{}/{digest}", result.execution_id)).execute(&mut **tx).await?;
    Ok(digest)
}
