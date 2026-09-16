//! Explicit disposable-database gate: never silently passes without PostgreSQL.
use chrono::{Duration, Utc};
use development_workflow_contract::*;
use light_workflow::{development_store::*, invocation::*};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;
use workflow_invocation_contract::{StartInvocationRequest, canonical_sha256};

struct Fixture {
    host: Uuid,
    binding: Uuid,
    feature: FeatureRun,
    claim: StageClaim,
    request: StartInvocationRequest,
    definition: Value,
}
impl Fixture {
    fn auth(&self) -> AuthenticatedInvocationContext<'_> {
        AuthenticatedInvocationContext {
            host_id: self.host,
            principal_subject: "gateway",
            end_user_subject: "owner",
            update_user: "gate",
            user_authorization: None,
            user_authorization_exp: None,
        }
    }
    fn prepared(&self) -> PreparedInvocationStart<'_> {
        PreparedInvocationStart {
            binding_id: self.binding,
            process_id: Uuid::now_v7(),
            initial_task_id: Uuid::now_v7(),
            application_id: "feature-design",
            initial_task_name: "author",
            initial_task_type: "run",
            definition_snapshot: &self.definition,
            execution_placement: "host",
            task_policy_digest: self.request.policy_digest.trim_start_matches("sha256:"),
            public_output_schema: None,
        }
    }
}
async fn fixture(pool: &PgPool) -> Fixture {
    fixture_stage(pool, false).await
}

async fn fixture_stage(pool: &PgPool, terminal: bool) -> Fixture {
    fixture_stage_variant(pool, terminal, false).await
}

async fn fixture_stage_variant(pool: &PgPool, terminal: bool, fixed_design: bool) -> Fixture {
    let host = Uuid::now_v7();
    let binding = Uuid::now_v7();
    let definition_id = Uuid::now_v7();
    let tool = Uuid::now_v7();
    let deadline = Utc::now() + Duration::hours(1);
    let mut feature: FeatureRun = serde_json::from_str(include_str!(
        "../../../contracts/development-workflow/v1/feature-run.json"
    ))
    .unwrap();
    feature.budgets.deadline_epoch_seconds = deadline.timestamp() as u64;
    if terminal {
        feature.allowed_stage.kind = StageKind::Finalize;
    }
    let definition = json!({"document":{"name":"feature-design","metadata":{
        "developmentWorkflowStage":feature.allowed_stage,
        "developmentWorkflowTurns":{"author":{"kind":"author","budgetScope":stage_budget_scope(&feature.allowed_stage).unwrap()}},
        "developmentWorkflowAcceptance":{"requiredReviewers":["claude"],"requiredChecks":["test"],"requireDesignSignoff":false,"requiredPublicationIntents":[]},
        "developmentWorkflowSuccessors": if terminal { json!([]) } else if fixed_design { json!([{"kind":"finalize","phaseId":null}]) } else { json!([{"kind":"plan","phaseId":null}]) },
        "developmentWorkflowTerminal":terminal
    }},"do":[{"author":{"run":{"shell":{"command":"true"}}}}]});
    let digest = format!(
        "sha256:{}",
        execution_runner_protocol::canonical_sha256(&definition).unwrap()
    );
    let claim = StageClaim {
        feature_run_id: feature.feature_run_id.clone(),
        transition_id: feature.transition_id.clone(),
        predecessor_version: feature.version,
        stage: feature.allowed_stage.clone(),
        inputs: feature.accepted_inputs.clone(),
        definition: ArtifactRef {
            id: definition_id.to_string(),
            digest: digest.clone(),
        },
        workspace_binding: feature.vm.runner_binding.clone(),
        deadline_epoch_seconds: deadline.timestamp() as u64,
    };
    let input = json!({"stageClaim":claim});
    let input_digest = canonical_sha256(&input).unwrap();
    let request: StartInvocationRequest = serde_json::from_value(json!({
        "contractVersion":1,"workflowInstanceId":Uuid::now_v7(),"stableToolRef":tool,
        "workflowDefinitionId":definition_id,"workflowVersion":"1.0.0","definitionDigest":digest,
        "schemaDigest":digest,"policyDigest":digest,"responsePolicyDigest":digest,
        "mode":"async","executionClass":"standard","permitDepth":0,"deadlineTs":deadline,
        "canonicalInputProfile":"rfc8785-safe-json-v1","normalizedInputDigest":input_digest,
        "input":input,"callerClaims":{"sub":"owner"},"correlationId":"development-gate",
        "idempotency":{"kind":"EXPLICIT","scopedKeyDigest":canonical_sha256(&json!(host)).unwrap(),"inputDigest":input_digest,"inFlightUntil":deadline,"resultReplayUntil":deadline+Duration::hours(1)},
        "budget":{"maximumTaskAttempts":20,"maximumNestedCalls":10,"maximumDelegationDepth":2,"maximumParallelism":1,"maximumRequestBytes":65536,"maximumIntermediateBytes":65536,"maximumResultBytes":65536,"maximumCostUnits":100}
    })).unwrap();
    request.validate(Utc::now()).unwrap();
    sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'development','feature-design','1.0.0',$3)")
        .bind(host).bind(definition_id).bind(serde_json::to_string(&definition).unwrap()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,runtime_bounds) VALUES($1,$2,$3,$4,'1.0.0',$5,$5,$5,$5,'async',1000,3600000,'standard','compact-json','{}','{}','{}')")
        .bind(host).bind(binding).bind(tool).bind(definition_id).bind(digest).execute(pool).await.unwrap();
    Fixture {
        host,
        binding,
        feature,
        claim,
        request,
        definition,
    }
}
async fn start(pool: &PgPool, f: &Fixture, request: &StartInvocationRequest) -> StageClaimReceipt {
    let mut tx = pool.begin().await.unwrap();
    let receipt = claim_and_start(&mut tx, &f.auth(), &f.claim, request, &f.prepared())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    receipt
}
async fn count(pool: &PgPool, host: Uuid, table: &str) -> i64 {
    // Table names are fixed by this test, never external input.
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE host_id=$1"))
        .bind(host)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn acceptance_and_replan(pool: &PgPool, terminal: bool) {
    acceptance_variant(pool, terminal, false).await;
}

async fn acceptance_variant(pool: &PgPool, terminal: bool, fixed_design: bool) {
    use light_workflow::{
        artifact_publish::{ArtifactPublication, publish_artifact},
        artifact_store::DurableArtifactStore,
        configuration::ArtifactSettings,
        development_handoff::*,
    };
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, BTreeSet};
    fn hash(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }
    let f = fixture_stage_variant(pool, terminal, fixed_design).await;
    let mut tx = pool.begin().await.unwrap();
    create_feature(&mut tx, &f.auth(), &f.feature)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let claim = start(pool, &f, &f.request).await;
    let root = tempfile::tempdir().unwrap();
    let store = DurableArtifactStore::from_configuration(&ArtifactSettings {
        backend: "filesystem".into(),
        filesystem_root: Some(root.path().to_owned()),
        bucket: None,
        endpoint: None,
        allow_http: false,
        prefix: "evidence".into(),
        retention_days: 30,
    })
    .unwrap()
    .unwrap();
    let mut repository = task_workspace::SnapshotRepository {
        base_commit: "a".repeat(40),
        tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
        files: BTreeMap::new(),
    };
    if fixed_design {
        let git_root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(git_root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "fixture Git command failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet"]);
        std::fs::write(git_root.path().join("design.md"), b"# Accepted Design\n").unwrap();
        git(&["add", "design.md"]);
        repository.tree = git(&["write-tree"]);
        repository.files.insert(
            "design.md".into(),
            task_workspace::SnapshotFile {
                executable: false,
                bytes: b"# Accepted Design\n".to_vec(),
            },
        );
    }
    let repositories = vec![task_workspace::RepositoryCheckpoint {
        repository: "repo".into(),
        head: repository.base_commit.clone(),
        index_digest: hash(b""),
        status_digest: hash(b""),
        files: repository
            .files
            .iter()
            .map(|(path, file)| task_workspace::FileEntry {
                path: path.clone(),
                digest: hash(&file.bytes),
                executable: file.executable,
            })
            .collect(),
    }];
    let package = task_workspace::SnapshotPackage {
        schema_version: 1,
        workspace_id: "workspace".into(),
        task_id: "task".into(),
        feature_id: f.feature.feature_run_id.clone(),
        stage_id: claim.stage_execution_id.clone(),
        snapshot_id: "candidate".into(),
        checkpoint: task_workspace::Checkpoint {
            digest: hash(&serde_json::to_vec(&repositories).unwrap()),
            repositories,
        },
        repositories: BTreeMap::from([("repo".into(), repository.clone())]),
    };
    let receipt = package.verified_receipt().unwrap();
    let mut refs = BTreeMap::new();
    for (name, bytes) in [
        ("package", serde_json::to_vec(&package).unwrap()),
        ("manifest", serde_json::to_vec(&repository).unwrap()),
        ("proof", b"proof".to_vec()),
        (
            "check",
            serde_json::to_vec(
                &json!({"candidate":receipt.package_digest,"check":"test","passed":true}),
            )
            .unwrap(),
        ),
        ("design", b"design v1".to_vec()),
    ] {
        let artifact_id = Uuid::now_v7();
        let digest = publish_artifact(
            pool,
            &store,
            ArtifactPublication {
                host_id: f.host,
                artifact_id,
                execution_id: Uuid::now_v7(),
                process_id: Some(claim.process_id.parse().unwrap()),
                task_id: None,
                logical_name: name,
                media_type: "application/json",
                producer: "development-gate",
                policy_digest: f.request.policy_digest.trim_start_matches("sha256:"),
                retain_until: Utc::now() + Duration::days(1),
                bytes: &bytes,
            },
        )
        .await
        .unwrap();
        refs.insert(
            name,
            ArtifactRef {
                id: artifact_id.to_string(),
                digest,
            },
        );
    }
    let binding = ReviewBinding {
        feature_run_id: f.feature.feature_run_id.clone(),
        review_id: "review-1".into(),
        stage_execution_id: claim.stage_execution_id.clone(),
        reviewer: Reviewer::Claude,
        session_id: "review-session".into(),
        candidate: receipt.package_digest.clone(),
        repositories: BTreeSet::from(["repo".into()]),
    };
    let charge = TurnCharge {
        logical_turn_id: binding.review_id.clone(),
        stage_execution_id: claim.stage_execution_id.clone(),
        budget_scope: stage_budget_scope(&f.claim.stage).unwrap(),
        kind: TurnKind::Review,
        remediation_round_id: None,
    };
    let review = ReviewResult {
        binding: binding.clone(),
        accepted: true,
        evidence: refs["proof"].clone(),
        existing_findings: vec![],
        new_findings: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    allocate_review(&mut tx, &f.auth(), binding).await.unwrap();
    let token = match reserve_turn(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &claim,
        &charge,
        &hash(b"review"),
    )
    .await
    .unwrap()
    {
        TurnReservation::Dispatch { token } => token,
        other => panic!("{other:?}"),
    };
    assert!(
        record_review(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &charge.logical_turn_id
        )
        .await
        .is_err()
    );
    complete_turn(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &charge.logical_turn_id,
        token,
        &json!({"reviewResult":review}),
    )
    .await
    .unwrap();
    let recorded = record_review(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &charge.logical_turn_id,
    )
    .await
    .unwrap();
    assert_eq!(
        record_review(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &charge.logical_turn_id
        )
        .await
        .unwrap(),
        recorded
    );
    tx.commit().await.unwrap();
    let result = StageResult {
        feature_run_id: f.feature.feature_run_id.clone(),
        claim: claim.clone(),
        stage: f.claim.stage.clone(),
        inputs: f.claim.inputs.clone(),
        outputs: BTreeMap::from([("design".into(), refs["design"].clone())]),
        candidate: CandidateSnapshot {
            feature_run_id: f.feature.feature_run_id.clone(),
            stage_execution_id: claim.stage_execution_id.clone(),
            task_id: package.task_id.clone(),
            candidate_digest: receipt.package_digest.clone(),
            checkpoint_digest: receipt.checkpoint_digest,
            repositories: BTreeMap::from([(
                "repo".into(),
                RepositorySnapshot {
                    base_commit: repository.base_commit.clone(),
                    tree: repository.tree.clone(),
                    content_manifest: refs["manifest"].clone(),
                },
            )]),
            package: refs["package"].clone(),
        },
        validation: ValidationReceipt {
            candidate: receipt.package_digest,
            required_checks: BTreeSet::from(["test".into()]),
            passed_checks: BTreeMap::from([("test".into(), refs["check"].clone())]),
        },
        review_ids: BTreeSet::from(["review-1".into()]),
        finding_ids: BTreeSet::new(),
        native_checkpoint: None,
        publication_receipts: vec![],
        accepted: true,
    };
    let request = AcceptStage {
        operation_id: Uuid::now_v7(),
        expected_version: claim.feature_version,
        result,
        next_stage: if terminal {
            None
        } else {
            Some(StageSelector {
                kind: if fixed_design {
                    StageKind::Finalize
                } else {
                    StageKind::Plan
                },
                phase_id: None,
            })
        },
    };
    let mut tx = pool.begin().await.unwrap();
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &request)
            .await
            .is_err(),
        "active process must block acceptance"
    );
    tx.rollback().await.unwrap();
    // This tests fixed host-only handoffs, not a live remote worker fence.
    sqlx::query(
        "UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=now() WHERE host_id=$1",
    )
    .bind(f.host)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE process_info_t SET status_code='C',completed_ts=now() WHERE host_id=$1")
        .bind(f.host)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE task_info_t SET status_code='C',completed_ts=now() WHERE host_id=$1")
        .bind(f.host)
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &request)
            .await
            .is_err(),
        "run/call tasks need remote fencing evidence"
    );
    tx.rollback().await.unwrap();
    sqlx::query("UPDATE task_info_t SET task_type='set' WHERE host_id=$1")
        .bind(f.host)
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let mut invalid = request.clone();
    invalid.next_stage = Some(StageSelector {
        kind: if fixed_design {
            StageKind::Plan
        } else {
            StageKind::Finalize
        },
        phase_id: None,
    });
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &invalid)
            .await
            .is_err()
    );
    invalid = request.clone();
    invalid
        .result
        .candidate
        .repositories
        .get_mut("repo")
        .unwrap()
        .tree = "b".repeat(40);
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &invalid)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let raw = refs["proof"].digest.trim_start_matches("sha256:");
    let path = root.path().join(format!(
        "evidence/tenants/{}/objects/sha256/{}/{raw}",
        f.host,
        &raw[..2]
    ));
    std::fs::write(&path, b"broken").unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &request)
            .await
            .is_err(),
        "corrupt evidence blocks acceptance"
    );
    assert_eq!(
        load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
            .version,
        claim.feature_version
    );
    tx.rollback().await.unwrap();
    std::fs::write(&path, b"proof").unwrap();
    let mut effect_tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO workflow_task_effect_t(host_id,workflow_instance_id,task_name,idempotency_key,request_digest) VALUES($1,$2,'publish','publish-1',$3)")
        .bind(f.host).bind(claim.workflow_instance_id.parse::<Uuid>().unwrap())
        .bind(hash(b"publish")).execute(&mut *effect_tx).await.unwrap();
    assert!(
        accept_stage(&mut effect_tx, &f.auth(), &store, &request)
            .await
            .is_err(),
        "uncertain fixed effect blocks acceptance and VM release"
    );
    effect_tx.rollback().await.unwrap();
    if !terminal {
        let mut terminal_tx = pool.begin().await.unwrap();
        let mut invalid = request.clone();
        invalid.next_stage = None;
        assert!(
            accept_stage(&mut terminal_tx, &f.auth(), &store, &invalid)
                .await
                .is_err(),
            "ordinary stage cannot terminate by omitting its successor"
        );
        terminal_tx.rollback().await.unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    let accepted = accept_stage(&mut tx, &f.auth(), &store, &request)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        accepted.state,
        if terminal {
            FeatureState::Completed
        } else {
            FeatureState::ReadyForNextStage
        }
    );
    assert_eq!(accepted.budgets.charges.len(), 1);
    assert!(accepted.active_claim.is_none());
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        accept_stage(&mut tx, &f.auth(), &store, &request)
            .await
            .unwrap(),
        accepted
    );
    let mut conflicting = request.clone();
    conflicting.next_stage = Some(StageSelector {
        kind: if fixed_design {
            StageKind::Plan
        } else {
            StageKind::Finalize
        },
        phase_id: None,
    });
    assert!(
        accept_stage(&mut tx, &f.auth(), &store, &conflicting)
            .await
            .is_err()
    );
    if fixed_design {
        tx.commit().await.unwrap();
        fixed_design_terminal(pool, &f, &store, &accepted).await;
        return;
    }
    if terminal {
        assert!(accepted.vm.released);
        assert!(!accepted.vm.release_pending);
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT feature_id FROM development_vm_t WHERE host_id=$1 AND vm_id=$2",
        )
        .bind(f.host)
        .bind(&accepted.vm.vm_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(holder.is_none());
        let mut replacement = f.feature.clone();
        replacement.feature_run_id = "replacement".into();
        replacement.vm.feature_run_id = "replacement".into();
        let replacement = create_feature(&mut tx, &f.auth(), &replacement)
            .await
            .unwrap();
        assert!(replacement.vm.generation > accepted.vm.generation);
        assert_eq!(
            accept_stage(&mut tx, &f.auth(), &store, &request)
                .await
                .unwrap(),
            accepted
        );
        let holder: Option<String> = sqlx::query_scalar(
            "SELECT feature_id FROM development_vm_t WHERE host_id=$1 AND vm_id=$2",
        )
        .bind(f.host)
        .bind(&accepted.vm.vm_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(holder.as_deref(), Some("replacement"));
        tx.commit().await.unwrap();
        return;
    }
    let replan = ReplanStage {
        operation_id: Uuid::now_v7(),
        feature_id: f.feature.feature_run_id.clone(),
        expected_version: accepted.version,
        stage: f.claim.stage.clone(),
        inputs: f.claim.inputs.clone(),
        reason: "revise design".into(),
    };
    let reopened = replan_stage(&mut tx, &f.auth(), &replan).await.unwrap();
    assert_eq!(reopened.budgets, accepted.budgets);
    assert_eq!(reopened.accepted_results, accepted.accepted_results);
    assert!(!reopened.accepted_inputs.contains_key("design"));
    assert_ne!(reopened.transition_id, accepted.transition_id);
    assert_eq!(
        replan_stage(&mut tx, &f.auth(), &replan).await.unwrap(),
        reopened
    );
    // Historical stage start replays its receipt, never creates a new author.
    assert_eq!(
        claim_and_start(&mut tx, &f.auth(), &f.claim, &f.request, &f.prepared())
            .await
            .unwrap(),
        claim
    );
    let mut stale = f.claim.clone();
    stale.transition_id = accepted.transition_id;
    stale.predecessor_version = accepted.version;
    stale.stage = accepted.allowed_stage;
    stale.inputs = accepted.accepted_inputs;
    assert!(
        claim_and_start(&mut tx, &f.auth(), &stale, &f.request, &f.prepared())
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert_eq!(count(pool, f.host, "development_stage_t").await, 1);
    let mut tx = pool.begin().await.unwrap();
    let cancelled = request_cancel(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        reopened.version,
    )
    .await
    .unwrap();
    assert_eq!(cancelled.state, FeatureState::Cancelled);
    assert!(
        cancelled.vm.released,
        "idle feature with verified historical stages can release"
    );
    tx.commit().await.unwrap();
}

async fn fixed_design_terminal(
    pool: &PgPool,
    source: &Fixture,
    store: &light_workflow::artifact_store::DurableArtifactStore,
    accepted: &FeatureRun,
) {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use development_workflow_contract::publication::*;
    use light_workflow::{development_finalize::complete_design, publication_dispatch::*};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let definition_id = Uuid::now_v7();
    let tool = Uuid::now_v7();
    let binding = Uuid::now_v7();
    let mut definition = source.definition.clone();
    let metadata = &mut definition["document"]["metadata"];
    metadata["developmentWorkflowStage"] = json!(accepted.allowed_stage);
    metadata["developmentWorkflowTurns"] = json!({});
    metadata["developmentWorkflowTerminal"] = json!(true);
    metadata["developmentWorkflowFinalizeAcceptedDesign"] = json!(true);
    metadata["developmentWorkflowSuccessors"] = json!([]);
    metadata["developmentWorkflowAcceptance"] = json!({"requiredReviewers":[],"requiredChecks":["design-shape"],"requireDesignSignoff":false,"requiredPublicationIntents":[]});
    metadata["developmentWorkflowDocumentChecks"] = json!({"design-shape":{"kind":"document-lines","repository":"repo","path":"design.md","requiredLines":["# Accepted Design"]}});
    metadata["developmentWorkflowPublication"] = json!({"policy":{"repositories":["o/r"],"documentBranches":[],"documentPaths":[],"allowIssues":true,"allowComments":false},"slots":{"design-issue":{"repository":"o/r","destination":{"kind":"issue","title":"Accepted design"},"sourceRepository":"repo","sourcePath":"design.md","revision":1}}});
    definition["do"] = json!([{"author":{"set":{"ready":true}}}]);
    let digest = canonical_sha256(&definition).unwrap();
    let claim = StageClaim {
        feature_run_id: accepted.feature_run_id.clone(),
        transition_id: accepted.transition_id.clone(),
        predecessor_version: accepted.version,
        stage: accepted.allowed_stage.clone(),
        inputs: accepted.accepted_inputs.clone(),
        definition: ArtifactRef {
            id: definition_id.to_string(),
            digest: digest.clone(),
        },
        workspace_binding: accepted.vm.runner_binding.clone(),
        deadline_epoch_seconds: accepted.budgets.deadline_epoch_seconds,
    };
    let input = json!({"stageClaim":claim});
    let input_digest = canonical_sha256(&input).unwrap();
    let mut value = serde_json::to_value(&source.request).unwrap();
    value["workflowInstanceId"] = json!(Uuid::now_v7());
    value["stableToolRef"] = json!(tool);
    value["workflowDefinitionId"] = json!(definition_id);
    for key in [
        "definitionDigest",
        "schemaDigest",
        "policyDigest",
        "responsePolicyDigest",
    ] {
        value[key] = json!(digest);
    }
    value["input"] = input;
    value["normalizedInputDigest"] = json!(input_digest);
    value["idempotency"]["inputDigest"] = json!(input_digest);
    value["idempotency"]["scopedKeyDigest"] = json!(canonical_sha256(&json!(tool)).unwrap());
    let request: StartInvocationRequest = serde_json::from_value(value).unwrap();
    sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'development','feature-finalize','1.0.0',$3)")
        .bind(source.host).bind(definition_id).bind(serde_json::to_string(&definition).unwrap()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,runtime_bounds) VALUES($1,$2,$3,$4,'1.0.0',$5,$5,$5,$5,'async',1000,3600000,'standard','compact-json','{}','{}','{}')")
        .bind(source.host).bind(binding).bind(tool).bind(definition_id).bind(&digest).execute(pool).await.unwrap();
    let finalize = Fixture {
        host: source.host,
        binding,
        feature: accepted.clone(),
        claim,
        request,
        definition,
    };
    let receipt = start(pool, &finalize, &finalize.request).await;
    // Test-owned fixed task completion, not a native completion receipt.
    sqlx::query("UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=now() WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(source.host).bind(receipt.workflow_instance_id.parse::<Uuid>().unwrap()).execute(pool).await.unwrap();
    sqlx::query("UPDATE process_info_t SET status_code='C' WHERE host_id=$1 AND process_id=$2")
        .bind(source.host)
        .bind(receipt.process_id.parse::<Uuid>().unwrap())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE task_info_t SET status_code='C',task_type='set' WHERE host_id=$1 AND process_id=$2",
    )
    .bind(source.host)
    .bind(receipt.process_id.parse::<Uuid>().unwrap())
    .execute(pool)
    .await
    .unwrap();
    let operation = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version,
            operation
        )
        .await
        .is_err(),
        "missing required publication must block finalization"
    );
    assert!(
        !load_feature(&mut tx, &source.auth(), &accepted.feature_run_id)
            .await
            .unwrap()
            .vm
            .released
    );
    tx.rollback().await.unwrap();
    #[derive(Clone, Default)]
    struct Remote {
        writes: Arc<AtomicUsize>,
        receipt: Arc<tokio::sync::Mutex<Option<ProviderPublicationReceipt>>>,
    }
    async fn deliver(
        State(remote): State<Remote>,
        Json(request): Json<PublicationDelivery>,
    ) -> StatusCode {
        remote.writes.fetch_add(1, Ordering::SeqCst);
        *remote.receipt.lock().await = Some(ProviderPublicationReceipt {
            key: request.key.clone(),
            request_digest: request.request_digest().unwrap(),
            provider_id: "11".into(),
            resource_url: "https://github.com/o/r/issues/1".into(),
            commit: None,
        });
        StatusCode::INTERNAL_SERVER_ERROR
    }
    async fn inspect(
        State(remote): State<Remote>,
        Json(request): Json<PublicationDelivery>,
    ) -> Json<ProviderPublicationReceipt> {
        let receipt = remote.receipt.lock().await.clone().unwrap();
        assert_eq!(receipt.key, request.key);
        assert_eq!(receipt.request_digest, request.request_digest().unwrap());
        Json(receipt)
    }
    let remote = Remote::default();
    let app = Router::new()
        .route("/v1/publications", post(deliver))
        .route("/v1/publications/status", post(inspect))
        .with_state(remote.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider =
        PublicationProvider::new(&format!("http://{address}/v1/"), "s".repeat(32)).unwrap();
    assert!(
        dispatch(
            pool,
            store,
            &provider,
            &source.auth(),
            &accepted.feature_run_id,
            "design-issue"
        )
        .await
        .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version,
            operation
        )
        .await
        .is_err(),
        "uncertain publication must retain VM"
    );
    tx.rollback().await.unwrap();
    let publication = dispatch(
        pool,
        store,
        &provider,
        &source.auth(),
        &accepted.feature_run_id,
        "design-issue",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        dispatch(
            pool,
            store,
            &provider,
            &source.auth(),
            &accepted.feature_run_id,
            "design-issue"
        )
        .await
        .unwrap(),
        Some(publication)
    );
    assert_eq!(remote.writes.load(Ordering::SeqCst), 1);
    let mut tx = pool.begin().await.unwrap();
    let completed = complete_design(
        &mut tx,
        &source.auth(),
        store,
        &accepted.feature_run_id,
        receipt.feature_version,
        operation,
    )
    .await
    .unwrap();
    assert_eq!(completed.state, FeatureState::Completed);
    assert!(completed.vm.released);
    assert!(!completed.vm.release_pending);
    assert!(completed.active_claim.is_none());
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version,
            operation
        )
        .await
        .unwrap(),
        completed
    );
    assert!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version + 1,
            operation
        )
        .await
        .is_err()
    );
    assert!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version,
            Uuid::nil()
        )
        .await
        .is_err()
    );
    let mut replacement = source.feature.clone();
    replacement.feature_run_id = "finalize-replacement".into();
    replacement.vm.feature_run_id = replacement.feature_run_id.clone();
    let replacement = create_feature(&mut tx, &source.auth(), &replacement)
        .await
        .unwrap();
    assert!(replacement.vm.generation > completed.vm.generation);
    assert_eq!(
        complete_design(
            &mut tx,
            &source.auth(),
            store,
            &accepted.feature_run_id,
            receipt.feature_version,
            operation
        )
        .await
        .unwrap(),
        completed
    );
    let holder: Option<String> =
        sqlx::query_scalar("SELECT feature_id FROM development_vm_t WHERE host_id=$1 AND vm_id=$2")
            .bind(source.host)
            .bind(&completed.vm.vm_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(holder.as_deref(), Some("finalize-replacement"));
    tx.commit().await.unwrap();
    server.abort();
}

#[tokio::test]
#[ignore = "requires a fresh disposable PostgreSQL database; run the development stage-store gate"]
async fn atomic_claim_replay_conflict_rollback_and_dispatch_fencing() {
    let url = std::env::var("DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL")
        .expect("disposable PostgreSQL URL required");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    // CREATE (not IF NOT EXISTS): refuses to run against an existing workflow DB.
    sqlx::raw_sql("CREATE SCHEMA workflow_ops")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='operations_workflow_runtime') THEN CREATE ROLE operations_workflow_runtime; END IF; END $$").execute(&pool).await.unwrap();
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='operations_workflow_migrator') THEN CREATE ROLE operations_workflow_migrator; END IF; END $$").execute(&pool).await.unwrap();
    sqlx::raw_sql(workflow_store::MIGRATION_SQL)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(MIGRATION_SQL).execute(&pool).await.unwrap();
    sqlx::raw_sql(workflow_store::AGENT_DISPATCH_MIGRATION_SQL)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE operations_workflow_runtime")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let f = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    let feature = create_feature(&mut tx, &f.auth(), &f.feature)
        .await
        .unwrap();
    assert_eq!(feature.vm.generation, 1);
    tx.commit().await.unwrap();
    let mut request2 = f.request.clone();
    request2.workflow_instance_id = Uuid::now_v7();
    let (a, b) = tokio::join!(start(&pool, &f, &f.request), start(&pool, &f, &request2));
    assert_eq!(a, b);
    // Commit followed by a lost response uses the same stage/process/instance.
    assert_eq!(start(&pool, &f, &request2).await, a);
    publication_effect_journal(&pool, f.host, &a).await;
    for table in [
        "development_stage_t",
        "process_info_t",
        "task_info_t",
        "workflow_invocation_t",
        "workflow_invocation_budget_t",
    ] {
        assert_eq!(count(&pool, f.host, table).await, 1, "{table}");
    }
    let mut tx = pool.begin().await.unwrap();
    let mut changed = f.claim.clone();
    changed.deadline_epoch_seconds -= 1;
    assert!(
        claim_and_start(&mut tx, &f.auth(), &changed, &f.request, &f.prepared())
            .await
            .is_err()
    );
    let mut changed_request = f.request.clone();
    changed_request.budget.maximum_task_attempts += 1;
    assert!(
        claim_and_start(
            &mut tx,
            &f.auth(),
            &f.claim,
            &changed_request,
            &f.prepared()
        )
        .await
        .is_err()
    );
    let mut other = f.auth();
    other.end_user_subject = "other-user";
    assert!(
        load_feature(&mut tx, &other, &f.feature.feature_run_id)
            .await
            .is_err()
    );
    assert!(
        accept_invocation(&mut tx, &f.auth(), &f.request, &f.prepared())
            .await
            .is_err()
    );
    let mut other_feature = f.feature.clone();
    other_feature.feature_run_id = "other-feature".into();
    other_feature.vm.feature_run_id = "other-feature".into();
    assert!(
        create_feature(&mut tx, &f.auth(), &other_feature)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();

    let charge = TurnCharge {
        logical_turn_id: "author-1".into(),
        stage_execution_id: a.stage_execution_id.clone(),
        budget_scope: stage_budget_scope(&f.claim.stage).unwrap(),
        kind: TurnKind::Author,
        remediation_round_id: None,
    };
    let digest = canonical_sha256(&json!("author input")).unwrap();
    let mut tx = pool.begin().await.unwrap();
    let token = match reserve_turn(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &a,
        &charge,
        &digest,
    )
    .await
    .unwrap()
    {
        TurnReservation::Dispatch { token } => token,
        other => panic!("unexpected {other:?}"),
    };
    tx.commit().await.unwrap();
    // A lost send/result response must reconcile, not charge or dispatch again.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        reserve_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &a,
            &charge,
            &digest
        )
        .await
        .unwrap(),
        TurnReservation::Uncertain { token }
    );
    assert!(
        reserve_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &a,
            &charge,
            &canonical_sha256(&json!("changed")).unwrap()
        )
        .await
        .is_err()
    );
    let result = json!({"checkpoint":"saved","nativeThread":"thread-1"});
    assert!(
        complete_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &charge.logical_turn_id,
            Uuid::now_v7(),
            &result
        )
        .await
        .is_err()
    );
    complete_turn(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &charge.logical_turn_id,
        token,
        &result,
    )
    .await
    .unwrap();
    complete_turn(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        &charge.logical_turn_id,
        token,
        &result,
    )
    .await
    .unwrap();
    assert!(
        complete_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &charge.logical_turn_id,
            token,
            &json!({"changed":true})
        )
        .await
        .is_err()
    );
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        reserve_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &a,
            &charge,
            &digest
        )
        .await
        .unwrap(),
        TurnReservation::Replay { result }
    );
    assert_eq!(
        load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
            .budgets
            .charges
            .len(),
        1
    );
    tx.commit().await.unwrap();

    let rollback = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    create_feature(&mut tx, &rollback.auth(), &rollback.feature)
        .await
        .unwrap();
    claim_and_start(
        &mut tx,
        &rollback.auth(),
        &rollback.claim,
        &rollback.request,
        &rollback.prepared(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    for table in [
        "development_feature_t",
        "development_stage_t",
        "process_info_t",
        "task_info_t",
        "workflow_invocation_t",
        "workflow_invocation_idempotency_t",
    ] {
        assert_eq!(
            count(&pool, rollback.host, table).await,
            0,
            "rollback {table}"
        );
    }

    // Simulate the legacy event path, which does not use accept_invocation.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts,definition_snapshot) VALUES($1,$2,$3,$4,'test','Workflow','A',now(),$5)")
        .bind(f.host).bind(Uuid::now_v7()).bind(f.request.workflow_definition_id).bind(Uuid::now_v7().to_string()).bind(&f.definition).execute(&mut *tx).await.unwrap();
    assert!(
        tx.commit().await.is_err(),
        "unclaimed event process must not commit"
    );
    assert_eq!(count(&pool, f.host, "process_info_t").await, 1);
    sqlx::query("UPDATE task_info_t SET locked='Y' WHERE host_id=$1")
        .bind(f.host)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE task_info_t SET locked='N' WHERE host_id=$1")
        .bind(f.host)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let cancelled = request_cancel(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        a.feature_version,
    )
    .await
    .unwrap();
    assert_eq!(cancelled.state, FeatureState::VmReleasePending);
    assert!(!cancelled.vm.released);
    assert!(
        complete_turn(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id,
            &charge.logical_turn_id,
            token,
            &json!({})
        )
        .await
        .is_err()
    );
    tx.commit().await.unwrap();
    assert!(
        sqlx::query("UPDATE task_info_t SET locked='Y' WHERE host_id=$1")
            .bind(f.host)
            .execute(&pool)
            .await
            .is_err(),
        "fenced feature cannot dispatch"
    );
    let idle = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    let first = create_feature(&mut tx, &idle.auth(), &idle.feature)
        .await
        .unwrap();
    let released = request_cancel(&mut tx, &idle.auth(), &first.feature_run_id, first.version)
        .await
        .unwrap();
    assert_eq!(released.state, FeatureState::Cancelled);
    assert!(released.vm.released);
    let mut next = idle.feature.clone();
    next.feature_run_id = "next-feature".into();
    next.vm.feature_run_id = next.feature_run_id.clone();
    let next = create_feature(&mut tx, &idle.auth(), &next).await.unwrap();
    assert_eq!(next.vm.generation, first.vm.generation + 1);
    // Replaying the old cancellation cannot release the new reservation.
    assert_eq!(
        request_cancel(&mut tx, &idle.auth(), &first.feature_run_id, first.version)
            .await
            .unwrap(),
        released
    );
    let holder: String =
        sqlx::query_scalar("SELECT feature_id FROM development_vm_t WHERE host_id=$1 AND vm_id=$2")
            .bind(idle.host)
            .bind(&first.vm.vm_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(holder, next.feature_run_id);
    tx.commit().await.unwrap();
    acceptance_and_replan(&pool, false).await;
    acceptance_and_replan(&pool, true).await;
    acceptance_variant(&pool, false, true).await;
    runner_fence_bridge(&pool).await;
    native_dispatch_intent(&pool).await;
    authenticated_intake_transaction(&pool).await;
    transactional_artifact_publication(&url).await;
    pool.close().await;
}

async fn publication_effect_journal(pool: &PgPool, host: Uuid, claim: &StageClaimReceipt) {
    use development_workflow_contract::publication::{EffectState, PublicationEffect};
    use light_workflow::publication_journal::{PublicationClaim, PublicationJournalKey};
    let effect = PublicationEffect {
        id: format!("publication-{}", Uuid::now_v7()),
        request_digest: format!("sha256:{}", "a".repeat(64)),
        state: EffectState::Prepared,
    };
    let key = PublicationJournalKey {
        host_id: host,
        workflow_instance_id: claim.workflow_instance_id.parse().unwrap(),
        task_name: "publication:design:1",
        effect: &effect,
    };
    // Rollback before dispatch leaves no intent; a committed lost response does.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        key.claim(&mut tx).await.unwrap(),
        PublicationClaim::Dispatch
    );
    tx.rollback().await.unwrap();
    let first = async {
        let mut tx = pool.begin().await.unwrap();
        let result = key.claim(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        result
    };
    let second = async {
        let mut tx = pool.begin().await.unwrap();
        let result = key.claim(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        result
    };
    let (a, b) = tokio::join!(first, second);
    assert!(matches!(
        (&a, &b),
        (PublicationClaim::Dispatch, PublicationClaim::Reconcile)
            | (PublicationClaim::Reconcile, PublicationClaim::Dispatch)
    ));
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        key.claim(&mut tx).await.unwrap(),
        PublicationClaim::Reconcile
    );
    let mut changed = effect.clone();
    changed.request_digest = format!("sha256:{}", "b".repeat(64));
    let changed_key = PublicationJournalKey {
        effect: &changed,
        ..key
    };
    assert!(changed_key.claim(&mut tx).await.is_err());
    assert!(
        changed_key
            .confirm(&mut tx, &json!({"providerId":"wrong"}))
            .await
            .is_err()
    );
    let result =
        json!({"providerId":"test-provider:1","verifiedContentDigest":effect.request_digest});
    key.confirm(&mut tx, &result).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        key.claim(&mut tx).await.unwrap(),
        PublicationClaim::Reconcile
    );
    key.confirm(&mut tx, &result).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        key.claim(&mut tx).await.unwrap(),
        PublicationClaim::Confirmed(result.clone())
    );
    key.confirm(&mut tx, &result).await.unwrap();
    assert!(
        key.confirm(&mut tx, &json!({"providerId":"changed"}))
            .await
            .is_err()
    );
    assert!(key.confirm(&mut tx, &Value::Null).await.is_err());
    assert_eq!(
        key.claim(&mut tx).await.unwrap(),
        PublicationClaim::Confirmed(result)
    );
    tx.commit().await.unwrap();
}

async fn transactional_artifact_publication(url: &str) {
    use light_workflow::{
        artifact_publish::{ArtifactPublication, publish_artifact_in_transaction},
        artifact_store::DurableArtifactStore,
        configuration::ArtifactSettings,
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE operations_workflow_runtime")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = DurableArtifactStore::from_configuration(&ArtifactSettings {
        backend: "filesystem".into(),
        filesystem_root: Some(root.path().to_owned()),
        bucket: None,
        endpoint: None,
        allow_http: false,
        prefix: "evidence".into(),
        retention_days: 30,
    })
    .unwrap()
    .unwrap();
    let host = Uuid::now_v7();
    let artifact_id = Uuid::now_v7();
    let execution = Uuid::now_v7();
    let process = Uuid::now_v7();
    let task = Uuid::now_v7();
    let policy = "a".repeat(64);
    let publication = || ArtifactPublication {
        host_id: host,
        artifact_id,
        execution_id: execution,
        process_id: Some(process),
        task_id: Some(task),
        logical_name: "snapshot:gate",
        media_type: "application/json",
        producer: "workflow-manager-snapshot",
        policy_digest: &policy,
        retain_until: Utc::now() + Duration::days(1),
        bytes: b"{\"gate\":true}",
    };
    let mut tx = pool.begin().await.unwrap();
    let digest = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        publish_artifact_in_transaction(&mut tx, &store, publication()),
    )
    .await
    .expect("publication must not acquire a second connection")
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        count(&pool, host, "workflow_artifact_t").await,
        0,
        "result rollback must leave no visible artifact metadata"
    );
    // Recreate the filesystem client: retry uses retained bytes, not process memory.
    let store = DurableArtifactStore::from_configuration(&ArtifactSettings {
        backend: "filesystem".into(),
        filesystem_root: Some(root.path().to_owned()),
        bucket: None,
        endpoint: None,
        allow_http: false,
        prefix: "evidence".into(),
        retention_days: 30,
    })
    .unwrap()
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        publish_artifact_in_transaction(&mut tx, &store, publication())
            .await
            .unwrap(),
        digest
    );
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        publish_artifact_in_transaction(&mut tx, &store, publication())
            .await
            .unwrap(),
        digest
    );
    tx.commit().await.unwrap();
    assert_eq!(count(&pool, host, "workflow_artifact_t").await, 1);
    for variant in 0..3 {
        let mut changed = publication();
        match variant {
            0 => changed.bytes = b"changed",
            1 => changed.process_id = Some(Uuid::now_v7()),
            _ => changed.execution_id = Uuid::now_v7(),
        }
        let mut tx = pool.begin().await.unwrap();
        assert!(
            publish_artifact_in_transaction(&mut tx, &store, changed)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        store
            .read_verified(&host.to_string(), &digest, 1024)
            .await
            .unwrap(),
        publication().bytes
    );
    sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETE_PENDING' WHERE host_id=$1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        publish_artifact_in_transaction(&mut tx, &store, publication())
            .await
            .is_err(),
        "replay must not resurrect fenced evidence"
    );
    tx.rollback().await.unwrap();
    pool.close().await;
}

async fn authenticated_intake_transaction(pool: &PgPool) {
    use light_workflow::development_intake;
    let mut f = fixture(pool).await;
    f.claim.feature_run_id = Uuid::now_v7().to_string();
    f.claim.transition_id = Uuid::now_v7().to_string();
    f.claim.stage = StageSelector {
        kind: StageKind::Intake,
        phase_id: None,
    };
    f.claim.inputs.clear();
    f.definition["document"]["name"] = json!("feature-intake");
    f.definition["document"]["metadata"]["developmentWorkflowStage"] = json!(f.claim.stage);
    f.definition["document"]["metadata"]["developmentWorkflowIntake"] = json!({
        "vmId":"intake-pilot","workspaceBinding":f.claim.workspace_binding,
        "maximumTurns":12,"maximumRemediationRounds":3,"maximumDurationSeconds":3600
    });
    let digest = canonical_sha256(&f.definition).unwrap();
    f.claim.definition.digest = digest.clone();
    let mut input = json!({"stageClaim":f.claim,"featureIntake":{"issue":{
        "repository":"networknt/light-fabric","number":392,
        "url":"https://github.com/networknt/light-fabric/issues/392"}}});
    // An omitted optional phase is the same typed claim as phaseId:null.
    // Keep the exact submitted bytes for subsequent idempotency checks.
    input["stageClaim"]["stage"]
        .as_object_mut()
        .unwrap()
        .remove("phaseId");
    let input_digest = canonical_sha256(&input).unwrap();
    let mut request = serde_json::to_value(&f.request).unwrap();
    request["definitionDigest"] = json!(digest);
    request["input"] = input.clone();
    request["normalizedInputDigest"] = json!(input_digest);
    request["idempotency"]["inputDigest"] = json!(input_digest);
    f.request = serde_json::from_value(request).unwrap();
    f.request.validate(Utc::now()).unwrap();
    let mut retry = f.request.clone();
    retry.deadline_ts += Duration::seconds(10);
    development_intake::bind_deadline(&mut retry, &f.claim).unwrap();
    development_intake::bind_deadline(&mut f.request, &f.claim).unwrap();
    assert_eq!(retry.deadline_ts, f.request.deadline_ts);
    retry.deadline_ts -= Duration::seconds(1);
    assert!(development_intake::bind_deadline(&mut retry, &f.claim).is_err());
    let seed = development_intake::seed(
        &f.definition,
        &input,
        &f.claim,
        Utc::now().timestamp() as u64,
    )
    .unwrap()
    .unwrap();

    // Model a later grant/authority rejection: nothing becomes visible if the
    // encompassing invocation transaction rolls back after creation and claim.
    let mut tx = pool.begin().await.unwrap();
    development_intake::create_or_replay(&mut tx, &f.auth(), &seed)
        .await
        .unwrap();
    claim_and_start(&mut tx, &f.auth(), &f.claim, &f.request, &f.prepared())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(pool, f.host, "development_feature_t").await, 0);
    assert_eq!(count(pool, f.host, "development_vm_t").await, 0);
    assert_eq!(count(pool, f.host, "process_info_t").await, 0);

    async fn admit(pool: &PgPool, f: &Fixture, seed: &FeatureRun) -> StageClaimReceipt {
        let mut tx = pool.begin().await.unwrap();
        development_intake::create_or_replay(&mut tx, &f.auth(), seed)
            .await
            .unwrap();
        let result = claim_and_start(&mut tx, &f.auth(), &f.claim, &f.request, &f.prepared())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        result
    }
    let (a, b) = tokio::join!(admit(pool, &f, &seed), admit(pool, &f, &seed));
    assert_eq!(a, b);
    assert_eq!(count(pool, f.host, "development_feature_t").await, 1);
    assert_eq!(count(pool, f.host, "process_info_t").await, 1);
    let mut tx = pool.begin().await.unwrap();
    let mut foreign = f.auth();
    foreign.end_user_subject = "different-owner";
    assert!(
        development_intake::create_or_replay(&mut tx, &foreign, &seed)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut changed = seed.clone();
    changed.issue.number += 1;
    let mut tx = pool.begin().await.unwrap();
    assert!(
        development_intake::create_or_replay(&mut tx, &f.auth(), &changed)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();

    // Simulate confirmed terminal release and a replacement generation. An old
    // intake replay must not reacquire or update the replacement VM holder.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE development_vm_t SET feature_id='replacement',generation=generation+1 WHERE host_id=$1")
        .bind(f.host).execute(&mut *tx).await.unwrap();
    development_intake::create_or_replay(&mut tx, &f.auth(), &seed)
        .await
        .unwrap();
    let holder: String =
        sqlx::query_scalar("SELECT feature_id FROM development_vm_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(holder, "replacement");
    tx.rollback().await.unwrap();
}

async fn native_dispatch_intent(pool: &PgPool) {
    native_review_allocation(pool).await;
    let f = fixture(pool).await;
    let mut tx = pool.begin().await.unwrap();
    create_feature(&mut tx, &f.auth(), &f.feature)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let claim = start(pool, &f, &f.request).await;
    let process = claim.process_id.parse().unwrap();
    let task: Uuid =
        sqlx::query_scalar("SELECT task_id FROM task_info_t WHERE host_id=$1 AND process_id=$2")
            .bind(f.host)
            .bind(process)
            .fetch_one(pool)
            .await
            .unwrap();
    let agent = Uuid::now_v7();
    let deadline = f.request.deadline_ts - Duration::minutes(1);
    let input = json!({"workspace":{"instruction":"draft"}});
    let schema = json!({"type":"object"});
    for _ in 0..2 {
        assert_eq!(
            light_workflow::native_jobs::enqueue(
                pool,
                f.host,
                process,
                task,
                "author",
                agent,
                input.clone(),
                schema.clone(),
                deadline,
                100,
                0,
                0,
                2
            )
            .await
            .unwrap(),
            task
        );
    }
    assert_eq!(count(pool, f.host, "workflow_agent_job_t").await, 1);
    assert_eq!(count(pool, f.host, "development_turn_t").await, 1);
    // Retry may not widen budget or change input or Agent, even before delivery.
    assert!(
        light_workflow::native_jobs::enqueue(
            pool,
            f.host,
            process,
            task,
            "author",
            agent,
            input.clone(),
            schema.clone(),
            deadline,
            101,
            0,
            0,
            2
        )
        .await
        .is_err()
    );
    assert!(
        light_workflow::native_jobs::enqueue(
            pool,
            f.host,
            process,
            task,
            "author",
            Uuid::now_v7(),
            input.clone(),
            schema.clone(),
            deadline,
            100,
            0,
            0,
            2
        )
        .await
        .is_err()
    );
    assert!(
        light_workflow::native_jobs::enqueue(
            pool,
            Uuid::now_v7(),
            process,
            task,
            "author",
            agent,
            input.clone(),
            schema.clone(),
            deadline,
            100,
            0,
            0,
            2
        )
        .await
        .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    let feature = load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
        .await
        .unwrap();
    assert_eq!(feature.budgets.charges.len(), 1);
    let instance = f.request.workflow_instance_id;
    sqlx::query("UPDATE workflow_invocation_t SET cancellation_policy='DISABLED' WHERE host_id=$1")
        .bind(f.host)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        light_workflow::development_cancel::cancel_invocation(&mut tx, &f.auth(), instance)
            .await
            .unwrap()
    );
    assert_eq!(
        load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
            .state,
        FeatureState::Active
    );
    sqlx::query("UPDATE workflow_invocation_t SET cancellation_policy='BEFORE_EFFECTS_ONLY',effect_state='confirmed' WHERE host_id=$1")
        .bind(f.host).execute(&mut *tx).await.unwrap();
    assert!(
        light_workflow::development_cancel::cancel_invocation(&mut tx, &f.auth(), instance)
            .await
            .unwrap()
    );
    assert_eq!(
        load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
            .state,
        FeatureState::Active
    );
    sqlx::query(
        "UPDATE workflow_invocation_t SET cancellation_policy='COOPERATIVE' WHERE host_id=$1",
    )
    .bind(f.host)
    .execute(&mut *tx)
    .await
    .unwrap();
    let mut foreign = f.auth();
    foreign.end_user_subject = "other-owner";
    assert!(
        light_workflow::development_cancel::cancel_invocation(&mut tx, &foreign, instance)
            .await
            .is_err()
    );
    assert!(
        !light_workflow::development_cancel::cancel_invocation(&mut tx, &f.auth(), Uuid::now_v7())
            .await
            .unwrap()
    );
    assert!(
        light_workflow::development_cancel::cancel_invocation(&mut tx, &f.auth(), instance)
            .await
            .unwrap()
    );
    assert_eq!(
        load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
            .state,
        FeatureState::VmReleasePending
    );
    tx.commit().await.unwrap();
    assert!(
        light_workflow::native_jobs::enqueue(
            pool, f.host, process, task, "author", agent, input, schema, deadline, 100, 0, 0, 2
        )
        .await
        .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        !light_workflow::development_cancel::finalize(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    // A validated not-dispatched report is a positive fence, unlike a timeout.
    let report = light_client::workflow_job_transport::Report {
        host_id: f.host,
        job_id: task,
        state: "CANCELLED".into(),
        output: None,
        error: None,
        cleanup: Some(json!({"kind":"not-dispatched","jobId":task})),
    };
    light_workflow::development_cancel::validate_cleanup(&report, "workflow-agent").unwrap();
    sqlx::query("UPDATE workflow_agent_job_t SET state='CANCELLED',report=$3 WHERE host_id=$1 AND job_id=$2")
        .bind(f.host).bind(task).bind(serde_json::to_value(report).unwrap()).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        light_workflow::development_cancel::finalize(&mut tx, &f.auth(), &f.feature.feature_run_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    sqlx::query("UPDATE development_vm_t SET feature_id='replacement',generation=generation+1 WHERE host_id=$1")
        .bind(f.host).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        !light_workflow::development_cancel::finalize(
            &mut tx,
            &f.auth(),
            &f.feature.feature_run_id
        )
        .await
        .unwrap()
    );
    let holder: String =
        sqlx::query_scalar("SELECT feature_id FROM development_vm_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(holder, "replacement");
    tx.commit().await.unwrap();
}

async fn native_review_allocation(pool: &PgPool) {
    use light_workflow::{
        artifact_publish::{ArtifactPublication, publish_artifact},
        artifact_store::DurableArtifactStore,
        configuration::ArtifactSettings,
    };
    for (delivery, review_case) in [
        ("inline", "preamble"),
        ("checkpoint-workspace", "binding"),
        ("inline", "ledger"),
        ("checkpoint-workspace", "valid"),
    ] {
        let mut f = fixture(pool).await;
        let agent = Uuid::now_v7();
        f.definition["document"]["metadata"]["developmentWorkflowTurns"]["author"] = json!({
            "kind":"review", "budgetScope":stage_budget_scope(&f.feature.allowed_stage).unwrap(),
            "reviewer":"claude", "agentDefId":agent
        });
        let digest = canonical_sha256(&f.definition).unwrap();
        f.claim.definition.digest = digest.clone();
        f.request.definition_digest = digest;
        f.request.input = json!({"stageClaim":f.claim});
        f.request.normalized_input_digest = canonical_sha256(&f.request.input).unwrap();
        f.request.idempotency.input_digest = f.request.normalized_input_digest.clone();
        let mut tx = pool.begin().await.unwrap();
        create_feature(&mut tx, &f.auth(), &f.feature)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let claim = start(pool, &f, &f.request).await;
        let process: Uuid = claim.process_id.parse().unwrap();
        let task: Uuid = sqlx::query_scalar(
            "SELECT task_id FROM task_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(f.host)
        .bind(process)
        .fetch_one(pool)
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = DurableArtifactStore::from_configuration(&ArtifactSettings {
            backend: "filesystem".into(),
            filesystem_root: Some(root.path().to_owned()),
            bucket: None,
            endpoint: None,
            allow_http: false,
            prefix: "evidence".into(),
            retention_days: 30,
        })
        .unwrap()
        .unwrap();
        let repositories = vec![task_workspace::RepositoryCheckpoint {
            repository: "repo".into(),
            head: "a".repeat(40),
            index_digest: canonical_sha256(&json!(null)).unwrap(),
            status_digest: canonical_sha256(&json!(null)).unwrap(),
            files: vec![],
        }];
        let package = task_workspace::SnapshotPackage {
            schema_version: 1,
            workspace_id: "workspace".into(),
            task_id: "task".into(),
            feature_id: f.feature.feature_run_id.clone(),
            stage_id: claim.stage_execution_id.clone(),
            snapshot_id: "candidate".into(),
            checkpoint: task_workspace::Checkpoint {
                digest: workspace_execution_protocol::sha256(
                    &serde_json::to_vec(&repositories).unwrap(),
                ),
                repositories,
            },
            repositories: std::collections::BTreeMap::from([(
                "repo".into(),
                task_workspace::SnapshotRepository {
                    base_commit: "a".repeat(40),
                    tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
                    files: Default::default(),
                },
            )]),
        };
        let bytes = serde_json::to_vec(&package).unwrap();
        let artifact = Uuid::now_v7();
        let digest = publish_artifact(
            pool,
            &store,
            ArtifactPublication {
                host_id: f.host,
                artifact_id: artifact,
                execution_id: Uuid::now_v7(),
                process_id: Some(process),
                task_id: None,
                logical_name: "review-candidate",
                media_type: "application/json",
                producer: "development-gate",
                policy_digest: f.request.policy_digest.trim_start_matches("sha256:"),
                retain_until: Utc::now() + Duration::days(1),
                bytes: &bytes,
            },
        )
        .await
        .unwrap();
        let binding = json!({"featureRunId":f.feature.feature_run_id,
        "reviewId":format!("{}:author",claim.stage_execution_id),
        "stageExecutionId":claim.stage_execution_id,"reviewer":"claude","sessionId":task,
        "candidate":digest,"repositories":["repo"]});
        let input = json!({"reviewBinding":binding,"reviewArtifacts":{"contentDelivery":delivery,"candidate":{"id":artifact,"digest":digest},"before":{"id":artifact,"digest":digest}},
        "workspace":{"workspaceId":"workspace","task":{"kind":"existing","taskId":"task"},
            "intent":"review","expectedCheckpointDigest":package.checkpoint.digest,"instruction":"Review retained evidence."}});
        let enqueue = |input, selected_agent| {
            light_workflow::native_jobs::enqueue_with_artifacts(
                pool,
                f.host,
                process,
                task,
                "author",
                selected_agent,
                input,
                json!({"type":"object"}),
                f.request.deadline_ts - Duration::minutes(1),
                100,
                0,
                0,
                2,
                Some(&store),
            )
        };
        assert!(enqueue(input.clone(), Uuid::now_v7()).await.is_err());
        assert!(
            light_workflow::native_jobs::enqueue(
                pool,
                f.host,
                process,
                task,
                "author",
                agent,
                input.clone(),
                json!({"type":"object"}),
                f.request.deadline_ts - Duration::minutes(1),
                100,
                0,
                0,
                2
            )
            .await
            .is_err()
        );
        for (path, value) in [
            ("/reviewBinding/reviewId", json!("worker-chosen-review")),
            (
                "/reviewArtifacts/contentDelivery",
                json!("unverified-workspace"),
            ),
            ("/reviewArtifacts/candidate/id", json!(Uuid::now_v7())),
            ("/reviewArtifacts/before/id", json!(Uuid::now_v7())),
            (
                "/reviewArtifacts/candidate/digest",
                json!(format!("sha256:{}", "b".repeat(64))),
            ),
            ("/workspace/expectedCheckpointDigest", json!("wrong")),
            ("/workspace/task/taskId", json!("wrong")),
            ("/workspace/workspaceId", json!("wrong")),
        ] {
            let mut changed = input.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert!(enqueue(changed, agent).await.is_err(), "{path}");
        }
        for (column, value, restore) in [
            ("verification_state", "PENDING", "VERIFIED"),
            ("promotion_state", "STAGED", "BOUND"),
            ("deletion_state", "DELETED", "RETAINED"),
        ] {
            // Fixed test-owned column names only, never runtime/user SQL.
            let query = format!(
                "UPDATE workflow_artifact_t SET {column}=$3 WHERE host_id=$1 AND artifact_id=$2"
            );
            sqlx::query(&query)
                .bind(f.host)
                .bind(artifact)
                .bind(value)
                .execute(pool)
                .await
                .unwrap();
            assert!(enqueue(input.clone(), agent).await.is_err());
            sqlx::query(&query)
                .bind(f.host)
                .bind(artifact)
                .bind(restore)
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE workflow_artifact_t SET retain_until_ts=now()-interval '1 hour' WHERE host_id=$1 AND artifact_id=$2")
        .bind(f.host).bind(artifact).execute(pool).await.unwrap();
        assert!(enqueue(input.clone(), agent).await.is_err());
        sqlx::query("UPDATE workflow_artifact_t SET retain_until_ts=now()+interval '1 day',process_id=NULL WHERE host_id=$1 AND artifact_id=$2")
        .bind(f.host).bind(artifact).execute(pool).await.unwrap();
        assert!(enqueue(input.clone(), agent).await.is_err());
        sqlx::query(
            "UPDATE workflow_artifact_t SET process_id=$3 WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(f.host)
        .bind(artifact)
        .bind(process)
        .execute(pool)
        .await
        .unwrap();
        let hex = digest.strip_prefix("sha256:").unwrap();
        let object = root.path().join(format!(
            "evidence/tenants/{}/objects/sha256/{}/{hex}",
            f.host,
            &hex[..2]
        ));
        std::fs::write(&object, b"corrupt").unwrap();
        assert!(enqueue(input.clone(), agent).await.is_err());
        std::fs::remove_file(&object).unwrap();
        assert!(enqueue(input.clone(), agent).await.is_err());
        std::fs::write(&object, &bytes).unwrap();
        let mut forged = input.clone();
        forged["verifiedReviewMaterial"] = json!({});
        assert!(enqueue(forged, agent).await.is_err());
        let mut missing_before = input.clone();
        missing_before["workspace"]["thread"] = json!({"mode":"resume"});
        missing_before["reviewArtifacts"]
            .as_object_mut()
            .unwrap()
            .remove("before");
        assert!(enqueue(missing_before, agent).await.is_err());
        let mut oversized = input.clone();
        oversized["workspace"]["instruction"] = json!("x".repeat(65536));
        assert!(enqueue(oversized, agent).await.is_err());
        assert_eq!(count(pool, f.host, "development_turn_t").await, 0);
        assert_eq!(count(pool, f.host, "workflow_agent_job_t").await, 0);
        for _ in 0..2 {
            enqueue(input.clone(), agent).await.unwrap();
        }
        assert_eq!(count(pool, f.host, "development_turn_t").await, 1);
        assert_eq!(count(pool, f.host, "workflow_agent_job_t").await, 1);
        let stored: Value = sqlx::query_scalar(
            "SELECT input FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2",
        )
        .bind(f.host)
        .bind(task)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(stored["verifiedReviewMaterial"]["candidateDigest"], digest);
        assert_eq!(
            stored["verifiedReviewMaterial"]["reviewBinding"],
            stored["reviewBinding"]
        );
        assert_eq!(
            stored["verifiedReviewMaterial"]["delta"]["repo"],
            if delivery == "inline" {
                json!([])
            } else {
                json!("")
            }
        );
        if delivery == "checkpoint-workspace" {
            assert!(
                stored["verifiedReviewMaterial"]
                    .get("repositories")
                    .is_none()
            );
            assert_eq!(
                stored["verifiedReviewMaterial"]["manifest"][0]["repository"],
                "repo"
            );
        }
        assert!(
            stored["workspace"]["instruction"]
                .as_str()
                .unwrap()
                .contains("Workflow-verified review material")
        );
        let ledger: Value = sqlx::query_scalar(
            "SELECT finding_ledger FROM development_feature_t WHERE host_id=$1 AND feature_id=$2",
        )
        .bind(f.host)
        .bind(&f.feature.feature_run_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let ledger: FindingLedger = serde_json::from_value(ledger).unwrap();
        assert_eq!(ledger.allocated_reviews.len(), 1);
        assert_eq!(
            serde_json::to_value(ledger.allocated_reviews.values().next().unwrap()).unwrap(),
            binding
        );
        review_report_completion(
            pool,
            &f,
            agent,
            task,
            &binding,
            artifact,
            &digest,
            review_case,
        )
        .await;
    }
}

async fn review_report_completion(
    pool: &PgPool,
    f: &Fixture,
    agent: Uuid,
    task: Uuid,
    binding: &Value,
    artifact: Uuid,
    digest: &str,
    case: &str,
) {
    use axum::http::StatusCode;
    use light_workflow::job_authorization::persist_verified_report;
    let mut review = json!({"binding":binding,"accepted":true,
        "evidence":{"id":artifact,"digest":digest},"existingFindings":[],"newFindings":[]});
    if case == "binding" {
        review["binding"]["candidate"] = json!("wrong");
    }
    if case == "ledger" {
        review["existingFindings"] = json!([{
        "existingFindingId":"unknown-finding","disposition":"verified-resolved",
        "evidence":{"id":artifact,"digest":digest}}]);
    }
    let answer = if case == "preamble" {
        format!("Unrequested prose.\n{review}")
    } else {
        review.to_string()
    };
    let execution = Uuid::now_v7();
    let turn = Uuid::now_v7();
    let output = json!({"finalMessage":answer});
    let normalized = json!({"executionId":execution,"origin":{"kind":"agent","serviceId":"review-agent","instanceId":"test","hostId":f.host},
        "subject":{"kind":"agent-turn","session_id":task,"subject_id":turn,"turn_id":turn},
        "attempt":1,"state":"SUCCEEDED","exitCode":0,"startedAt":Utc::now(),"finishedAt":Utc::now(),"durationMs":1,
        "stdout":{"truncated":false,"originalBytes":0},"stderr":{"truncated":false,"originalBytes":0},
        "structuredOutput":output,"artifacts":[],"backendOperationId":"test-review",
        "cleanupState":"CONFIRMED","policyDigest":f.request.policy_digest,"compatibilityDigest":"compat",
        "definitionDigest":f.request.definition_digest,"commandTemplateDigest":"template","retrySafety":"inspect-required","evidence":{}});
    let report = light_client::workflow_job_transport::Report {
        host_id: f.host,
        job_id: task,
        state: "SUCCEEDED".into(),
        output: Some(json!({"result":normalized,"executionId":execution,"fencingToken":1})),
        error: None,
        cleanup: None,
    };
    // Malformed content does not authorize a different peer, subject or fence.
    assert_eq!(
        persist_verified_report(pool, None, "other-agent", agent, report.clone()).await,
        Err(StatusCode::CONFLICT)
    );
    let mut bad = report.clone();
    bad.output.as_mut().unwrap()["fencingToken"] = json!(0);
    assert_eq!(
        persist_verified_report(pool, None, "review-agent", agent, bad).await,
        Err(StatusCode::CONFLICT)
    );
    assert_eq!(
        persist_verified_report(pool, None, "review-agent", Uuid::now_v7(), report.clone()).await,
        Err(StatusCode::FORBIDDEN)
    );
    for _ in 0..2 {
        assert_eq!(
            persist_verified_report(pool, None, "review-agent", agent, report.clone()).await,
            Ok(StatusCode::NO_CONTENT)
        );
    }
    let (state,public,error,stored):(String,Option<Value>,Option<Value>,Value)=sqlx::query_as(
        "SELECT state,public_output,error,report FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2")
        .bind(f.host).bind(task).fetch_one(pool).await.unwrap();
    assert_eq!(stored, serde_json::to_value(&report).unwrap());
    assert_eq!(
        state,
        if case == "valid" {
            "SUCCEEDED"
        } else {
            "FAILED"
        }
    );
    if case == "valid" {
        assert_eq!(public, Some(output));
        assert!(error.is_none());
    } else {
        assert!(public.is_none());
        assert_eq!(error.unwrap()["code"], "NATIVE_REVIEW_OUTPUT_INVALID");
    }
    let completed: bool = sqlx::query_scalar(
        "SELECT completed_ts IS NOT NULL FROM development_turn_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(f.host)
    .bind(task)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(completed);
    assert_eq!(
        count(pool, f.host, "development_execution_fence_t").await,
        1
    );
    let ledger: Value = sqlx::query_scalar(
        "SELECT finding_ledger FROM development_feature_t WHERE host_id=$1 AND feature_id=$2",
    )
    .bind(f.host)
    .bind(&f.feature.feature_run_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let ledger: FindingLedger = serde_json::from_value(ledger).unwrap();
    assert_eq!(ledger.reviews.len(), usize::from(case == "valid"));
    let mut changed = report.clone();
    changed.output.as_mut().unwrap()["result"]["structuredOutput"]["finalMessage"] =
        json!("different");
    assert_eq!(
        persist_verified_report(pool, None, "review-agent", agent, changed).await,
        Err(StatusCode::CONFLICT)
    );
    let mut tx = pool.begin().await.unwrap();
    let feature = load_feature(&mut tx, &f.auth(), &f.feature.feature_run_id)
        .await
        .unwrap();
    request_cancel(&mut tx, &f.auth(), &feature.feature_run_id, feature.version)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Lost response remains replayable after cancellation/stage state changes.
    assert_eq!(
        persist_verified_report(pool, None, "review-agent", agent, report).await,
        Ok(StatusCode::NO_CONTENT)
    );
}

async fn runner_fence_bridge(pool: &PgPool) {
    use execution_runner_protocol::*;
    use light_workflow::{development_execution::*, repositories::TerminalAttempt};
    use std::collections::BTreeMap;
    let f = fixture(pool).await;
    let mut tx = pool.begin().await.unwrap();
    create_feature(&mut tx, &f.auth(), &f.feature)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let claim = start(pool, &f, &f.request).await;
    let task: Uuid = sqlx::query_scalar("SELECT task_id FROM task_info_t WHERE host_id=$1")
        .bind(f.host)
        .fetch_one(pool)
        .await
        .unwrap();
    let charge = TurnCharge {
        logical_turn_id: "runner-author".into(),
        stage_execution_id: claim.stage_execution_id.clone(),
        budget_scope: stage_budget_scope(&f.claim.stage).unwrap(),
        kind: TurnKind::Author,
        remediation_round_id: None,
    };
    let digest = workflow_invocation_contract::canonical_sha256(&json!("runner request")).unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        reserve_task_turn(&mut tx, &f.auth(), &claim, Uuid::now_v7(), &charge, &digest)
            .await
            .is_err()
    );
    assert!(matches!(
        reserve_task_turn(&mut tx, &f.auth(), &claim, task, &charge, &digest)
            .await
            .unwrap(),
        TurnReservation::Dispatch { .. }
    ));
    tx.commit().await.unwrap();
    let execution = Uuid::now_v7();
    let process = claim.process_id.parse().unwrap();
    let output = json!({"checkpoint":"saved","nativeThread":"native-1"});
    let mut normalized = NormalizedExecutionResult {
        execution_id: ExecutionId(execution),
        origin: AuthenticatedOrigin {
            kind: OriginKind::Workflow,
            service_id: "light-workflow".into(),
            instance_id: "test".into(),
            host_id: f.host,
        },
        subject: ExecutionSubject::WorkflowTask {
            subject_id: task,
            process_id: process,
            task_id: task,
        },
        attempt: 1,
        state: AttemptState::Succeeded,
        failure_class: None,
        exit_code: Some(0),
        signal: None,
        started_at: Utc::now(),
        finished_at: Utc::now(),
        duration_ms: 1,
        stdout: NormalizedOutput {
            inline: None,
            reference: None,
            truncated: false,
            original_bytes: 0,
        },
        stderr: NormalizedOutput {
            inline: None,
            reference: None,
            truncated: false,
            original_bytes: 0,
        },
        structured_output: Some(output.clone()),
        artifacts: vec![],
        backend_operation_id: "worker-1".into(),
        cleanup_state: CleanupState::Required,
        policy_digest: f.request.policy_digest.clone(),
        compatibility_digest: "compat".into(),
        definition_digest: f.request.definition_digest.clone(),
        command_template_digest: "template".into(),
        retry_safety: RetrySafety::InspectRequired,
        evidence: BTreeMap::new(),
    };
    let mut attempt = TerminalAttempt {
        host_id: f.host,
        execution_id: execution,
        request_id: Uuid::now_v7(),
        process_id: process,
        task_id: task,
        attempt_number: 1,
        lease_id: Uuid::now_v7(),
        fencing_token: 1,
        state: "SUCCEEDED".into(),
        normalized_result: Some(serde_json::to_value(&normalized).unwrap()),
        normalized_error: None,
    };
    let mut tx = pool.begin().await.unwrap();
    assert!(
        reconcile_runner_result(&mut tx, &attempt).await.is_err(),
        "success without cleanup is not a fence"
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        count(pool, f.host, "development_execution_fence_t").await,
        0
    );
    normalized.cleanup_state = CleanupState::Confirmed;
    normalized.policy_digest = format!("sha256:{}", "b".repeat(64));
    attempt.normalized_result = Some(serde_json::to_value(&normalized).unwrap());
    let mut tx = pool.begin().await.unwrap();
    assert!(
        reconcile_runner_result(&mut tx, &attempt).await.is_err(),
        "foreign policy rejected"
    );
    tx.rollback().await.unwrap();
    normalized.policy_digest = f.request.policy_digest.clone();
    attempt.normalized_result = Some(serde_json::to_value(&normalized).unwrap());
    let mut tx = pool.begin().await.unwrap();
    reconcile_runner_result(&mut tx, &attempt).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        count(pool, f.host, "development_execution_fence_t").await,
        0,
        "fence and result share workflow transaction"
    );
    let mut tx = pool.begin().await.unwrap();
    reconcile_runner_result(&mut tx, &attempt).await.unwrap();
    sqlx::query("UPDATE task_info_t SET accepted_attempt=1 WHERE host_id=$1 AND task_id=$2")
        .bind(f.host)
        .bind(task)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        runner_result_already_recorded(&mut tx, &attempt)
            .await
            .unwrap()
    );
    let old_token = attempt.fencing_token;
    attempt.fencing_token += 1;
    assert!(
        !runner_result_already_recorded(&mut tx, &attempt)
            .await
            .unwrap()
    );
    attempt.fencing_token = old_token;
    reconcile_runner_result(&mut tx, &attempt).await.unwrap();
    assert_eq!(
        reserve_task_turn(&mut tx, &f.auth(), &claim, task, &charge, &digest)
            .await
            .unwrap(),
        TurnReservation::Replay { result: output }
    );
    request_cancel(
        &mut tx,
        &f.auth(),
        &f.feature.feature_run_id,
        claim.feature_version,
    )
    .await
    .unwrap();
    // Cleanup may replay during cancellation, but no new output is published.
    reconcile_runner_result(&mut tx, &attempt).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        count(pool, f.host, "development_execution_fence_t").await,
        1
    );
}
