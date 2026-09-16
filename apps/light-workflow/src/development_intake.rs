//! Gateway-compatible development admission. Only the published definition
//! chooses a VM/binding and resource ceilings; input never contains FeatureRun.
use crate::{development_store::*, invocation::AuthenticatedInvocationContext};
use development_workflow_contract::{
    ArtifactRef, BudgetLedger, FeatureRun, FeatureState, IssueRef, StageClaim, StageKind,
    StageSelector, VmReservation,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use workflow_invocation_contract::canonical_sha256;

/// Gateway deadlines are relative to each HTTP attempt. Bind development work
/// to the claim's immutable absolute deadline without widening the Gateway's
/// incoming authority. Otherwise a lost-response retry changes its fingerprint.
pub fn bind_deadline(
    request: &mut workflow_invocation_contract::StartInvocationRequest,
    claim: &StageClaim,
) -> Result<(), StoreError> {
    let seconds = i64::try_from(claim.deadline_epoch_seconds)
        .map_err(|_| StoreError::Conflict("invalid stage deadline"))?;
    let deadline = chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or(StoreError::Conflict("invalid stage deadline"))?;
    check(
        deadline <= request.deadline_ts,
        "stage deadline exceeds incoming authority",
    )?;
    request.deadline_ts = deadline;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IntakePolicy {
    vm_id: String,
    workspace_binding: ArtifactRef,
    maximum_turns: u32,
    maximum_remediation_rounds: u32,
    maximum_duration_seconds: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IntakeInput {
    issue: IssueRef,
}

/// The ordinary Gateway start endpoint and explicit development endpoint share
/// the same pinned-definition gate. Unmarked workflows cannot use this lane.
pub fn resolve_claim(
    definition: &Value,
    input: &Value,
    explicit: Option<StageClaim>,
) -> Result<Option<StageClaim>, StoreError> {
    if !is_development_definition(definition) {
        check(
            explicit.is_none() && input.get("featureIntake").is_none(),
            "development admission requires a marked definition",
        )?;
        return Ok(None);
    }
    let claim: StageClaim = serde_json::from_value(input.get("stageClaim").cloned().ok_or(
        StoreError::Conflict("development stage requires atomic feature claim"),
    )?)?;
    if let Some(explicit) = explicit {
        check(
            explicit == claim,
            "development claim envelope differs from input",
        )?;
    }
    Ok(Some(claim))
}

/// Produces pristine state only. Initial intake cannot smuggle accepted evidence,
/// historical results, a generation, an owner, or a larger resource ceiling.
pub fn seed(
    definition: &Value,
    input: &Value,
    claim: &StageClaim,
    now: u64,
) -> Result<Option<FeatureRun>, StoreError> {
    let Some(raw) = input.get("featureIntake") else {
        return Ok(None);
    };
    let intake: IntakeInput = serde_json::from_value(raw.clone())?;
    let policy: IntakePolicy = serde_json::from_value(
        definition
            .pointer("/document/metadata/developmentWorkflowIntake")
            .cloned()
            .ok_or(StoreError::Conflict("published intake policy required"))?,
    )?;
    let stage: StageSelector = serde_json::from_value(
        definition
            .pointer("/document/metadata/developmentWorkflowStage")
            .cloned()
            .ok_or(StoreError::Conflict("published intake stage required"))?,
    )?;
    check(
        stage == claim.stage
            && stage.kind == StageKind::Intake
            && stage.phase_id.is_none()
            && claim.predecessor_version == 1
            && claim.inputs.is_empty(),
        "initial intake must have a pristine intake claim",
    )?;
    for id in [&claim.feature_run_id, &claim.transition_id] {
        check(
            uuid::Uuid::parse_str(id).is_ok_and(|v| !v.is_nil() && v.to_string() == *id),
            "intake identities must be canonical non-nil UUIDs",
        )?;
    }
    policy.workspace_binding.validate()?;
    check(
        !policy.vm_id.trim().is_empty()
            && policy.vm_id.len() <= 256
            && policy.workspace_binding == claim.workspace_binding
            && policy.maximum_turns > 0
            && policy.maximum_remediation_rounds > 0
            && policy.maximum_duration_seconds > 0
            && claim.deadline_epoch_seconds > now
            && claim.deadline_epoch_seconds - now <= policy.maximum_duration_seconds,
        "intake binding or resource policy rejected",
    )?;
    let parts: Vec<_> = intake.issue.repository.split('/').collect();
    check(
        parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
            && intake.issue.number > 0
            && intake.issue.url
                == format!(
                    "https://github.com/{}/issues/{}",
                    intake.issue.repository, intake.issue.number
                ),
        "intake issue reference rejected",
    )?;
    Ok(Some(FeatureRun {
        schema_version: 1,
        feature_run_id: claim.feature_run_id.clone(),
        issue: intake.issue,
        version: 1,
        state: FeatureState::ReadyForNextStage,
        transition_id: claim.transition_id.clone(),
        allowed_stage: stage,
        accepted_inputs: Default::default(),
        active_claim: None,
        claims: Default::default(),
        accepted_results: Vec::new(),
        budgets: BudgetLedger {
            maximum_turns: policy.maximum_turns,
            maximum_remediation_rounds: policy.maximum_remediation_rounds,
            deadline_epoch_seconds: claim.deadline_epoch_seconds,
            charges: Default::default(),
        },
        vm: VmReservation {
            vm_id: policy.vm_id,
            runner_binding: policy.workspace_binding,
            feature_run_id: claim.feature_run_id.clone(),
            generation: 0,
            acquired_epoch_seconds: 0,
            release_pending: false,
            released: false,
        },
    }))
}

/// Historical retries compare the immutable creation fingerprint even after VM
/// release. They never reacquire a slot now held by another feature.
pub async fn create_or_replay(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    seed: &FeatureRun,
) -> Result<(), StoreError> {
    let saved: Option<(String, String, String)> = sqlx::query_as(
        "SELECT principal_subject,end_user_subject,creation_digest FROM development_feature_t
         WHERE host_id=$1 AND feature_id=$2 FOR UPDATE",
    )
    .bind(auth.host_id)
    .bind(&seed.feature_run_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((principal, user, digest)) = saved {
        check(
            principal == auth.principal_subject && user == auth.end_user_subject,
            "feature unavailable for owner",
        )?;
        check(
            digest
                == canonical_sha256(&serde_json::to_value(seed)?)
                    .map_err(|_| StoreError::Conflict("invalid intake fingerprint"))?,
            "feature creation replay changed inputs",
        )?;
    } else {
        create_feature(tx, auth, seed).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (Value, Value, StageClaim) {
        let binding = ArtifactRef {
            id: "workspace-policy".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
        };
        let claim = StageClaim {
            feature_run_id: uuid::Uuid::now_v7().to_string(),
            transition_id: uuid::Uuid::now_v7().to_string(),
            predecessor_version: 1,
            stage: StageSelector {
                kind: StageKind::Intake,
                phase_id: None,
            },
            inputs: Default::default(),
            definition: binding.clone(),
            workspace_binding: binding.clone(),
            deadline_epoch_seconds: 200,
        };
        let definition = json!({"document":{"name":"feature-intake","metadata":{
            "developmentWorkflowStage":claim.stage,
            "developmentWorkflowIntake":{"vmId":"pilot","workspaceBinding":binding,
                "maximumTurns":12,"maximumRemediationRounds":3,"maximumDurationSeconds":100}
        }}});
        let input = json!({"stageClaim":claim,"featureIntake":{"issue":{
            "repository":"networknt/light-fabric","number":392,
            "url":"https://github.com/networknt/light-fabric/issues/392"}}});
        (definition, input, claim)
    }

    #[test]
    fn gateway_claim_and_explicit_route_share_the_same_envelope() {
        let (def, input, claim) = fixture();
        assert_eq!(
            resolve_claim(&def, &input, None).unwrap(),
            Some(claim.clone())
        );
        assert_eq!(
            resolve_claim(&def, &input, Some(claim.clone())).unwrap(),
            Some(claim.clone())
        );
        let mut wrong = claim;
        wrong.predecessor_version = 2;
        assert!(resolve_claim(&def, &input, Some(wrong)).is_err());
        assert!(resolve_claim(&def, &json!({}), None).is_err());
        assert!(resolve_claim(&json!({}), &input, None).is_err());
        assert!(
            resolve_claim(&json!({}), &json!({"message":"ordinary"}), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn intake_is_pristine_policy_bounded_and_deterministic() {
        let (def, input, claim) = fixture();
        let feature = seed(&def, &input, &claim, 100).unwrap().unwrap();
        assert_eq!(feature.budgets.maximum_turns, 12);
        assert_eq!(feature.vm.generation, 0);
        assert!(feature.claims.is_empty() && feature.accepted_results.is_empty());
        assert_eq!(feature, seed(&def, &input, &claim, 101).unwrap().unwrap());
        assert!(seed(&def, &input, &claim, 99).is_err());
        assert!(seed(&def, &input, &claim, 200).is_err());
        let mut injected = input.clone();
        injected["featureIntake"]["maximumTurns"] = json!(999);
        assert!(seed(&def, &injected, &claim, 100).is_err());
        injected = input.clone();
        injected["featureIntake"]["issue"]["url"] = json!("https://attacker.test/392");
        assert!(seed(&def, &injected, &claim, 100).is_err());
        let mut other = claim.clone();
        other.workspace_binding.id = "other-vm".into();
        assert!(seed(&def, &input, &other, 100).is_err());
        other = claim.clone();
        other.stage.kind = StageKind::Design;
        assert!(seed(&def, &input, &other, 100).is_err());
        other = claim;
        other
            .inputs
            .insert("forged".into(), other.definition.clone());
        assert!(seed(&def, &input, &other, 100).is_err());
    }
}
