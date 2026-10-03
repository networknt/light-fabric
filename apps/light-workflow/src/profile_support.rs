//! Execution compatibility is separate from admission permission and lifecycle cleanup.
use crate::executor::expression_completion::{self, ProfileDisposition};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportedProfiles {
    v2: bool,
}

impl SupportedProfiles {
    pub fn from_evaluator(available: bool) -> Self {
        Self { v2: available }
    }
    pub fn profiles(&self) -> Vec<&'static str> {
        if self.v2 {
            vec!["cel-workflow-v1", "cel-workflow-v2"]
        } else {
            vec!["cel-workflow-v1"]
        }
    }
    pub(crate) fn check(
        &self,
        profile: &str,
        snapshot: Option<&Value>,
        digest: Option<&str>,
    ) -> ProfileDisposition {
        // Integrity has precedence even when this worker cannot execute the profile.
        let identity = expression_completion::identity(profile, snapshot, digest, true);
        if identity == ProfileDisposition::Corrupt {
            return identity;
        }
        if !self.profiles().contains(&profile) {
            return ProfileDisposition::Deferred;
        }
        identity
    }
}

/// Include visible mismatches as well as supported work; don't hide corruption or
/// fill a bounded candidate window with consistent unsupported work.
pub(crate) fn eligible(alias: &str, parameter: &str) -> String {
    format!(
        "({alias}.expression_profile=ANY({parameter}::text[]) OR {})",
        mismatch(alias)
    )
}
pub(crate) fn mismatch(alias: &str) -> String {
    format!(
        "({alias}.expression_profile IS DISTINCT FROM COALESCE({alias}.definition_snapshot #>> '{{document,metadata,lightExpressionProfile}}','cel-workflow-v1') OR ({alias}.expression_profile='cel-workflow-v2' AND {alias}.definition_snapshot IS NULL) OR ({alias}.definition_snapshot #> '{{document,metadata,lightExpressionProfile}}' IS NOT NULL AND jsonb_typeof({alias}.definition_snapshot #> '{{document,metadata,lightExpressionProfile}}')<>'string'))"
    )
}

/// Fresh work only. Lifecycle sweeps/cleanup deliberately do not use this.
pub(crate) fn live_selection(alias: &str, compensation: &str) -> String {
    format!(
        "({alias}.active AND {alias}.status_code IN ('A','W') AND ({alias}.deadline_ts IS NULL OR {alias}.deadline_ts>clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM workflow_invocation_t i WHERE i.host_id={alias}.host_id AND i.process_id={alias}.process_id AND (((i.cancel_requested_ts IS NOT NULL OR i.state NOT IN ('ACCEPTED','RUNNING','WAITING')) AND NOT ({compensation} AND i.state='COMPENSATING')) OR (i.deadline_ts<=clock_timestamp() AND COALESCE(i.response_policy_snapshot->'privateExecutionProfile'->>'version','')<>'1'))))"
    )
}

pub(crate) async fn read(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    support: &SupportedProfiles,
) -> Result<ProfileDisposition, sqlx::Error> {
    let (profile,snapshot,digest):(String,Option<Value>,Option<String>)=sqlx::query_as("SELECT expression_profile,definition_snapshot,definition_digest FROM process_info_t WHERE host_id=$1 AND process_id=$2")
        .bind(host).bind(process).fetch_one(&mut **tx).await?;
    Ok(support.check(&profile, snapshot.as_ref(), digest.as_deref()))
}

/// Success callers propagate deferral so their entire transaction rolls back.
/// Corruption is an explicit, sanitized terminal failure, never a retry.
pub(crate) async fn success(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    support: &SupportedProfiles,
) -> Result<bool, sqlx::Error> {
    match read(tx, host, process, support).await? {
        ProfileDisposition::Deferred => Err(sqlx::Error::Protocol(
            "WORKFLOW_EXPRESSION_UNAVAILABLE".into(),
        )),
        ProfileDisposition::Corrupt => {
            expression_completion::reject_step(
                tx,
                host,
                process,
                task,
                "/definition_snapshot/expression_profile",
            )
            .await?;
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Mutable definitions are allowed only for pre-snapshot legacy processes.
pub(crate) fn require_legacy_fallback(profile: &str) -> Result<(), sqlx::Error> {
    if profile == "cel-workflow-v1" {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol(
            "EVALUATOR_PROFILE_UNSUPPORTED".into(),
        ))
    }
}
