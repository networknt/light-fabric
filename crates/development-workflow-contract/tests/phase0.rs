use development_workflow_contract::*;
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn worker_wire_schemas_match_rust_fixtures_and_reject_unknown_fields() {
    for (schema, fixture, review) in [
        (
            include_str!("../../../contracts/development-workflow/v1/review-result.schema.json"),
            include_str!("../../../contracts/development-workflow/v1/review-result.json"),
            true,
        ),
        (
            include_str!("../../../contracts/development-workflow/v1/remediation.schema.json"),
            include_str!("../../../contracts/development-workflow/v1/remediation.json"),
            false,
        ),
    ] {
        let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(fixture).unwrap();
        assert!(validator.is_valid(&value));
        if review {
            serde_json::from_value::<ReviewResult>(value.clone()).unwrap();
        } else {
            serde_json::from_value::<Remediation>(value.clone()).unwrap();
        }
        value["permissionOverride"] = true.into();
        assert!(!validator.is_valid(&value));
        if review {
            assert!(serde_json::from_value::<ReviewResult>(value).is_err());
        } else {
            assert!(serde_json::from_value::<Remediation>(value).is_err());
        }
    }
}

fn d(s: &str) -> String {
    identity("fixture", &[s])
}
fn artifact(s: &str) -> ArtifactRef {
    ArtifactRef {
        id: format!("artifact:{s}"),
        digest: d(s),
    }
}
fn snapshot(s: &str) -> CandidateSnapshot {
    CandidateSnapshot {
        feature_run_id: "feature-1".into(),
        stage_execution_id: "final".into(),
        task_id: "task".into(),
        candidate_digest: d(s),
        checkpoint_digest: d(&format!("checkpoint:{s}")),
        repositories: BTreeMap::from([(
            "repo".into(),
            RepositorySnapshot {
                base_commit: "a".repeat(40),
                tree: "b".repeat(40),
                content_manifest: artifact(s),
            },
        )]),
        package: artifact(s),
    }
}
fn checks(s: &str) -> ValidationReceipt {
    ValidationReceipt {
        candidate: d(s),
        required_checks: BTreeSet::from(["test".into()]),
        passed_checks: BTreeMap::from([("test".into(), artifact("test"))]),
    }
}
fn result(id: &str, role: Reviewer, candidate: &str) -> ReviewResult {
    ReviewResult {
        binding: ReviewBinding {
            feature_run_id: "feature-1".into(),
            review_id: id.into(),
            stage_execution_id: "final".into(),
            reviewer: role,
            session_id: format!("{role:?}-session"),
            candidate: d(candidate),
            repositories: BTreeSet::from(["repo".into()]),
        },
        accepted: true,
        evidence: artifact(id),
        existing_findings: vec![],
        new_findings: vec![],
    }
}
fn new_finding() -> NewFinding {
    NewFinding {
        local_id: "local-1".into(),
        repository: "repo".into(),
        location: "src/lib.rs:10".into(),
        failure: "incorrect boundary".into(),
        resolution: "reject expired authority".into(),
        evidence: artifact("failure"),
        severity: Severity::Blocking,
        duplicate_of: None,
        duplicate_reason: None,
    }
}
fn apply(ledger: &mut FindingLedger, result: ReviewResult) -> ReviewReceipt {
    ledger.allocate_review(result.binding.clone()).unwrap();
    ledger.apply_review(result).unwrap()
}
fn with_finding() -> (FindingLedger, String) {
    let mut ledger = FindingLedger::new("feature-1".into());
    let mut first = result("first", Reviewer::Claude, "A");
    first.accepted = false;
    first.new_findings.push(new_finding());
    let receipt = apply(&mut ledger, first);
    (ledger, receipt.canonical_ids["local-1"].clone())
}
fn verdict(receipt: &ReviewReceipt) -> ReviewVerdict {
    ReviewVerdict {
        reviewer: receipt.result.binding.reviewer,
        review_id: receipt.result.binding.review_id.clone(),
        session_id: receipt.result.binding.session_id.clone(),
        candidate: receipt.result.binding.candidate.clone(),
        accepted: receipt.result.accepted,
        evidence: receipt.result.evidence.clone(),
    }
}
fn local_fix() -> (FindingLedger, ReviewCoverage) {
    let mut ledger = FindingLedger::new("feature-1".into());
    let codex = apply(&mut ledger, result("codex-full", Reviewer::Codex, "A"));
    let mut r = result("claude-full", Reviewer::Claude, "A");
    r.accepted = false;
    r.new_findings.push(new_finding());
    let claude = apply(&mut ledger, r);
    let id = claude.canonical_ids["local-1"].clone();
    let mut fix = result("claude-resume", Reviewer::Claude, "B");
    fix.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::VerifiedResolved,
        evidence: artifact("fix"),
    });
    let fixed = apply(&mut ledger, fix);
    let coverage = ReviewCoverage {
        baseline: d("A"),
        snapshots: BTreeMap::from([(d("A"), snapshot("A")), (d("B"), snapshot("B"))]),
        full_reviews: vec![verdict(&codex), verdict(&claude)],
        transitions: vec![CoverageTransition {
            delta: DeltaReceipt {
                before: d("A"),
                after: d("B"),
                full_delta: artifact("A-B"),
                changed_paths: BTreeSet::from(["repo/src/lib.rs".into()]),
                affected_phases: BTreeSet::from(["phase-1".into()]),
                cross_repository_contract: false,
                requirements_or_design: false,
                security_or_public_api: false,
                migration: false,
                uncertain_impact: false,
                outside_fix_scope: false,
            },
            validation: checks("B"),
            active_reviewer: Reviewer::Claude,
            reviews: vec![verdict(&fixed)],
            closed_finding_ids: BTreeSet::from([id]),
            carried_reviewers: BTreeSet::from([Reviewer::Codex]),
        }],
    };
    (ledger, coverage)
}

#[test]
fn replay_roundtrip_retains_canonical_identity_and_rejects_mutation() {
    let (mut ledger, id) = with_finding();
    let before = ledger.clone();
    let original = ledger.reviews["first"].result.clone();
    assert_eq!(
        ledger.apply_review(original.clone()).unwrap().canonical_ids["local-1"],
        id
    );
    assert_eq!(ledger, before);
    let mut changed = original;
    changed.new_findings[0].failure = "changed on retry".into();
    assert!(ledger.apply_review(changed).is_err());
    assert_eq!(ledger, before);
    let restored: FindingLedger =
        serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
    assert_eq!(restored, ledger);
}

#[test]
fn rewording_with_explicit_reference_preserves_identity_and_history() {
    let (mut ledger, id) = with_finding();
    let mut second = result("second", Reviewer::Claude, "A");
    second.accepted = false;
    let mut item = new_finding();
    item.failure = "same failure expressed differently".into();
    item.duplicate_of = Some(id.clone());
    item.duplicate_reason = Some("same boundary and failing input".into());
    second.new_findings.push(item);
    assert_eq!(apply(&mut ledger, second).canonical_ids["local-1"], id);
    assert_eq!(ledger.findings.len(), 1);
    assert_eq!(ledger.findings[&id].state, FindingState::Open);
    assert_eq!(ledger.findings[&id].history.len(), 2);
}

#[test]
fn invalid_duplicate_and_conflicting_dispositions_are_atomic() {
    let (mut ledger, id) = with_finding();
    for duplicate in ["other-feature", "local-2"] {
        let mut r = result(duplicate, Reviewer::Claude, "A");
        r.accepted = false;
        let mut f = new_finding();
        f.duplicate_of = Some(duplicate.into());
        f.duplicate_reason = Some("same".into());
        r.new_findings.push(f);
        ledger.allocate_review(r.binding.clone()).unwrap();
        let before = ledger.clone();
        assert!(ledger.apply_review(r).is_err());
        assert_eq!(ledger, before);
    }
    let mut r = result("conflict", Reviewer::Claude, "A");
    for disposition in [Disposition::VerifiedResolved, Disposition::StillOpen] {
        r.existing_findings.push(ExistingFinding {
            existing_finding_id: id.clone(),
            disposition,
            evidence: artifact("e"),
        });
    }
    ledger.allocate_review(r.binding.clone()).unwrap();
    let before = ledger.clone();
    assert!(ledger.apply_review(r).is_err());
    assert_eq!(ledger, before);
}

#[test]
fn duplicate_cannot_close_or_reopen_a_finding() {
    let (mut ledger, id) = with_finding();
    let mut r = result("close", Reviewer::Claude, "A");
    r.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::VerifiedResolved,
        evidence: artifact("verified"),
    });
    apply(&mut ledger, r);
    let mut duplicate = result("duplicate", Reviewer::Claude, "A");
    duplicate.accepted = false;
    let mut f = new_finding();
    f.duplicate_of = Some(id.clone());
    f.duplicate_reason = Some("regression".into());
    duplicate.new_findings.push(f);
    ledger.allocate_review(duplicate.binding.clone()).unwrap();
    assert!(ledger.apply_review(duplicate).is_err());
    let mut reopen = result("reopen", Reviewer::Claude, "B");
    reopen.accepted = false;
    reopen.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::Reopened,
        evidence: artifact("regression"),
    });
    apply(&mut ledger, reopen);
    assert_eq!(ledger.findings[&id].state, FindingState::Open);
}

#[test]
fn dispute_requires_reviewer_or_authorized_human_disposition() {
    let (mut ledger, id) = with_finding();
    ledger
        .remediate(Remediation {
            turn_id: "fix".into(),
            candidate: d("A"),
            finding_id: id.clone(),
            changed_paths: BTreeSet::new(),
            evidence: artifact("dispute"),
            dispute: true,
        })
        .unwrap();
    assert!(!ledger.closed());
    assert_eq!(ledger.findings[&id].state, FindingState::Disputed);
    let mut wrong = result("wrong-role", Reviewer::Codex, "B");
    wrong.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::VerifiedResolved,
        evidence: artifact("fix"),
    });
    ledger.allocate_review(wrong.binding.clone()).unwrap();
    assert!(ledger.apply_review(wrong).is_err());
    let mut human = HumanDisposition {
        decision_id: "human".into(),
        finding_id: id,
        deferred: true,
        reason: "follow-up approved".into(),
        tracking_reference: "".into(),
        authority: artifact("human"),
    };
    assert!(ledger.dispose_by_human(human.clone()).is_err());
    human.tracking_reference = "issue:follow-up".into();
    ledger.dispose_by_human(human.clone()).unwrap();
    ledger.dispose_by_human(human).unwrap();
    assert!(ledger.closed());
}

#[test]
fn worker_cannot_select_a_new_review_identity_or_claim_empty_output_is_acceptance() {
    let mut ledger = FindingLedger::new("feature-1".into());
    assert!(
        ledger
            .apply_review(result("unallocated", Reviewer::Claude, "A"))
            .is_err()
    );
    assert!(serde_json::from_str::<ReviewResult>("{}").is_err());
    assert!(serde_json::from_str::<ReviewResult>(r#"{"accepted":true}"#).is_err());
    let mut value = serde_json::to_value(result("id", Reviewer::Claude, "A")).unwrap();
    value["selfClose"] = true.into();
    assert!(serde_json::from_value::<ReviewResult>(value).is_err());
}

#[test]
fn local_final_fix_closes_via_resumed_reviewer_and_explicit_carry() {
    let (ledger, coverage) = local_fix();
    coverage
        .validate_publication(&d("B"), &ledger, &checks("B"))
        .unwrap();
    assert_eq!(coverage.full_reviews[0].candidate, d("A"));
    let mut missing = coverage.clone();
    missing.transitions[0].carried_reviewers.clear();
    assert!(
        missing
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
    let mut forged = coverage;
    forged.full_reviews[0].candidate = d("B");
    assert!(
        forged
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
}

#[test]
fn broad_or_uncertain_change_requires_both_reviewers() {
    let (mut ledger, mut coverage) = local_fix();
    coverage.transitions[0].delta.security_or_public_api = true;
    assert!(
        coverage
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
    let other = apply(&mut ledger, result("codex-resume", Reviewer::Codex, "B"));
    coverage.transitions[0].reviews.push(verdict(&other));
    coverage.transitions[0].carried_reviewers.clear();
    coverage
        .validate_publication(&d("B"), &ledger, &checks("B"))
        .unwrap();
}

#[test]
fn missing_transition_snapshot_delta_or_check_blocks_publication() {
    let (ledger, coverage) = local_fix();
    let mut broken = coverage.clone();
    broken.transitions.clear();
    assert!(
        broken
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
    let mut broken = coverage.clone();
    broken.snapshots.remove(&d("A"));
    assert!(
        broken
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
    let mut broken = coverage.clone();
    broken.transitions[0].delta.full_delta.digest = "corrupt".into();
    assert!(
        broken
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
    let mut skipped = checks("B");
    skipped.passed_checks.clear();
    assert!(
        coverage
            .validate_publication(&d("B"), &ledger, &skipped)
            .is_err()
    );
    let mut broken = coverage;
    broken.transitions[0].reviews[0].session_id = "new conversation".into();
    assert!(
        broken
            .validate_publication(&d("B"), &ledger, &checks("B"))
            .is_err()
    );
}

#[test]
fn optional_signoff_is_exact_digest_and_feature_bound() {
    validate_signoff(false, "feature-1", &d("design"), None).unwrap();
    assert!(validate_signoff(true, "feature-1", &d("design"), None).is_err());
    let mut signoff = DesignSignoff {
        feature_run_id: "feature-1".into(),
        design_digest: d("design"),
        decision: SignoffDecision::Approved,
        authority: artifact("approval"),
    };
    validate_signoff(true, "feature-1", &d("design"), Some(&signoff)).unwrap();
    assert!(validate_signoff(true, "feature-1", &d("v2"), Some(&signoff)).is_err());
    assert!(validate_signoff(true, "feature-2", &d("design"), Some(&signoff)).is_err());
    for decision in [SignoffDecision::Rejected, SignoffDecision::ChangesRequested] {
        signoff.decision = decision;
        assert!(validate_signoff(true, "feature-1", &d("design"), Some(&signoff)).is_err());
    }
}

#[test]
fn reopened_phases_and_replacement_instances_keep_consumed_budget() {
    let mut budget = BudgetLedger::new(20, 100);
    for n in 0..3 {
        let charge = TurnCharge {
            logical_turn_id: format!("turn-{n}"),
            stage_execution_id: format!("reopened-stage-{n}"),
            budget_scope: "final".into(),
            kind: TurnKind::Remediation,
            remediation_round_id: Some(format!("round-{n}")),
        };
        budget.charge(charge.clone(), 1).unwrap();
        budget.charge(charge, 2).unwrap();
        budget = serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
    }
    assert_eq!(budget.charges.len(), 3);
    let exhausted = TurnCharge {
        logical_turn_id: "new-instance".into(),
        stage_execution_id: "replacement".into(),
        budget_scope: "final".into(),
        kind: TurnKind::Remediation,
        remediation_round_id: Some("round-4".into()),
    };
    assert!(budget.charge(exhausted, 3).is_err());
    let repair = TurnCharge {
        logical_turn_id: "repair".into(),
        stage_execution_id: "replacement".into(),
        budget_scope: "final".into(),
        kind: TurnKind::OutputRepair,
        remediation_round_id: Some("round-2".into()),
    };
    budget.charge(repair, 3).unwrap();
    assert_eq!(budget.charges.len(), 4);
    budget.maximum_turns = 4;
    let fix = TurnCharge {
        logical_turn_id: "validation-fix".into(),
        stage_execution_id: "replacement".into(),
        budget_scope: "final".into(),
        kind: TurnKind::ValidationFix,
        remediation_round_id: None,
    };
    assert!(budget.charge(fix, 3).is_err());
}

fn feature_and_claim() -> (FeatureRun, StageClaim) {
    let feature: FeatureRun = serde_json::from_str(include_str!(
        "../../../contracts/development-workflow/v1/feature-run.json"
    ))
    .unwrap();
    let claim: StageClaim = serde_json::from_str(include_str!(
        "../../../contracts/development-workflow/v1/stage-claim.json"
    ))
    .unwrap();
    (feature, claim)
}

#[test]
fn fixture_handoff_replays_historical_claim_before_stale_checks() {
    let (mut feature, claim) = feature_and_claim();
    let ClaimDecision::Admit {
        claim_id,
        request_digest,
    } = feature.check_claim(&claim, 1).unwrap()
    else {
        panic!()
    };
    let receipt = StageClaimReceipt {
        claim_id: claim_id.clone(),
        request_digest,
        stage_execution_id: "stage-1".into(),
        workflow_instance_id: "workflow-1".into(),
        process_id: "process-1".into(),
        feature_version: 2,
    };
    feature.claims.insert(claim_id, receipt.clone());
    feature.active_claim = Some(receipt.clone());
    feature.version = 9;
    feature.state = FeatureState::Completed;
    assert_eq!(
        feature.check_claim(&claim, 2000).unwrap(),
        ClaimDecision::Replay(receipt)
    );
    let mut changed = claim;
    changed.definition = artifact("changed");
    assert!(feature.check_claim(&changed, 1).is_err());
}

#[test]
fn stale_or_competing_stage_cannot_start() {
    let (mut feature, mut claim) = feature_and_claim();
    claim.predecessor_version = 0;
    assert!(feature.check_claim(&claim, 1).is_err());
    claim.predecessor_version = feature.version;
    claim.inputs.clear();
    assert!(feature.check_claim(&claim, 1).is_err());
    claim.inputs = feature.accepted_inputs.clone();
    feature.state = FeatureState::HumanResolutionRequired;
    assert!(feature.check_claim(&claim, 1).is_err());
    feature.state = FeatureState::ReadyForNextStage;
    claim.deadline_epoch_seconds = 2000;
    assert!(feature.check_claim(&claim, 1).is_err());
}

#[test]
fn vm_release_needs_terminal_fenced_evidence_for_same_generation() {
    let (feature, _) = feature_and_claim();
    let outstanding = BTreeSet::from(["execution".into(), "push".into()]);
    let mut receipt = VmReleaseReceipt {
        vm_id: feature.vm.vm_id.clone(),
        feature_run_id: feature.feature_run_id.clone(),
        generation: feature.vm.generation,
        terminal_state: FeatureState::Cancelled,
        dispatch_fenced: artifact("fenced"),
        execution_and_effect_receipts: BTreeMap::new(),
    };
    assert!(feature.vm.validate_release(&receipt, &outstanding).is_err());
    receipt.execution_and_effect_receipts = BTreeMap::from([
        ("execution".into(), artifact("stopped")),
        ("push".into(), artifact("reconciled")),
    ]);
    feature.vm.validate_release(&receipt, &outstanding).unwrap();
    receipt.generation += 1;
    assert!(feature.vm.validate_release(&receipt, &outstanding).is_err());
    receipt.generation -= 1;
    receipt.terminal_state = FeatureState::ReadyForNextStage;
    assert!(feature.vm.validate_release(&receipt, &outstanding).is_err());
}

#[test]
fn immutable_publication_revisions_and_monotonic_comment_intents() {
    let mut document = DocumentRevision {
        feature_run_id: "feature".into(),
        document_id: "design".into(),
        revision: 1,
        accepted_content: artifact("v1"),
        supersedes: None,
        publication_task_id: String::new(),
    };
    document.publication_task_id = document.task_identity();
    document.validate().unwrap();
    let first = document.publication_task_id.clone();
    document.revision = 2;
    document.supersedes = Some(artifact("v1"));
    document.accepted_content = artifact("v2");
    assert!(document.validate().is_err());
    document.publication_task_id = document.task_identity();
    document.validate().unwrap();
    assert_ne!(first, document.publication_task_id);
    let old = CommentIntent {
        feature_run_id: "feature".into(),
        purpose: CommentPurpose::Status,
        version: 1,
        body: artifact("body-1"),
    };
    let mut newer = old.clone();
    newer.version = 2;
    newer.body = artifact("body-2");
    newer.check_update(&old).unwrap();
    assert!(old.check_update(&newer).is_err());
    newer.version = 1;
    assert!(newer.check_update(&old).is_err());
    let mut round = old.clone();
    round.purpose = CommentPurpose::Round {
        stage_execution_id: "stage".into(),
        round: 1,
    };
    let mut rewrite = round.clone();
    rewrite.version = 2;
    rewrite.body = artifact("rewrite");
    assert!(rewrite.check_update(&round).is_err());
}

#[test]
fn stage_handoff_requires_durable_snapshot_and_current_owner() {
    let (mut feature, claim) = feature_and_claim();
    let ClaimDecision::Admit {
        claim_id,
        request_digest,
    } = feature.check_claim(&claim, 1).unwrap()
    else {
        panic!()
    };
    let receipt = StageClaimReceipt {
        claim_id,
        request_digest,
        stage_execution_id: "final".into(),
        workflow_instance_id: "wf".into(),
        process_id: "process".into(),
        feature_version: 2,
    };
    feature.active_claim = Some(receipt.clone());
    let mut result = StageResult {
        feature_run_id: feature.feature_run_id.clone(),
        claim: receipt,
        stage: feature.allowed_stage.clone(),
        inputs: feature.accepted_inputs.clone(),
        outputs: BTreeMap::from([("design".into(), artifact("design"))]),
        candidate: snapshot("A"),
        validation: checks("A"),
        review_ids: BTreeSet::from(["claude-review".into()]),
        finding_ids: BTreeSet::new(),
        native_checkpoint: None,
        publication_receipts: vec![],
        accepted: true,
    };
    result.validate_handoff(&feature).unwrap();
    let mut ledger = FindingLedger::new("feature-1".into());
    let policy = StageAcceptancePolicy {
        required_reviewers: BTreeSet::from([Reviewer::Claude]),
        required_checks: BTreeSet::from(["test".into()]),
        require_design_signoff: false,
        required_publication_intents: BTreeSet::new(),
    };
    assert!(
        result
            .validate_acceptance(&feature, &policy, &ledger, None)
            .is_err()
    );
    apply(
        &mut ledger,
        crate::result("claude-review", Reviewer::Claude, "A"),
    );
    result
        .validate_acceptance(&feature, &policy, &ledger, None)
        .unwrap();
    let original_feature = feature.clone();
    let original_result = result.clone();
    let mut source = result.clone();
    source.stage.kind = StageKind::Design;
    feature.accepted_results.push(source);
    feature.allowed_stage.kind = StageKind::Finalize;
    result.stage.kind = StageKind::Finalize;
    result.review_ids.clear();
    let fixed_policy = StageAcceptancePolicy {
        required_reviewers: BTreeSet::new(),
        required_checks: policy.required_checks.clone(),
        require_design_signoff: false,
        required_publication_intents: BTreeSet::new(),
    };
    result
        .validate_acceptance(&feature, &fixed_policy, &ledger, None)
        .unwrap();
    assert!(
        result
            .validate_acceptance(&feature, &policy, &ledger, None)
            .is_err(),
        "declared reviewers cannot be bypassed"
    );
    feature.accepted_results.last_mut().unwrap().stage.kind = StageKind::Plan;
    assert!(
        result
            .validate_acceptance(&feature, &fixed_policy, &ledger, None)
            .is_err(),
        "fixed design finalization cannot terminate later-phase work"
    );
    feature.accepted_results.clear();
    assert!(
        result
            .validate_acceptance(&feature, &fixed_policy, &ledger, None)
            .is_err(),
        "initial finalize has no reviewed design"
    );
    feature = original_feature;
    result = original_result;
    result.candidate.package.id.clear();
    assert!(result.validate_handoff(&feature).is_err());
    result.candidate = snapshot("A");
    feature.active_claim = None;
    assert!(result.validate_handoff(&feature).is_err());
}

#[test]
fn codex_fixes_before_claude_first_full_review() {
    let mut ledger = FindingLedger::new("feature-1".into());
    let mut first = result("codex-full", Reviewer::Codex, "A");
    first.accepted = false;
    first.new_findings.push(new_finding());
    let codex = apply(&mut ledger, first);
    let id = codex.canonical_ids["local-1"].clone();
    let mut resumed = result("codex-fix", Reviewer::Codex, "B");
    resumed.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::VerifiedResolved,
        evidence: artifact("fixed"),
    });
    let fix = apply(&mut ledger, resumed);
    let claude = apply(&mut ledger, result("claude-full", Reviewer::Claude, "B"));
    let (_, mut coverage) = local_fix();
    coverage.full_reviews = vec![verdict(&codex), verdict(&claude)];
    coverage.transitions[0].active_reviewer = Reviewer::Codex;
    coverage.transitions[0].reviews = vec![verdict(&fix)];
    coverage.transitions[0].carried_reviewers.clear();
    coverage.transitions[0].closed_finding_ids = BTreeSet::from([id]);
    coverage
        .validate_publication(&d("B"), &ledger, &checks("B"))
        .unwrap();
}

#[test]
fn nonprogress_counts_logical_reviews_without_replay_inflation() {
    let (mut ledger, id) = with_finding();
    let mut progress = NonProgress::default();
    for n in 1..=2 {
        let mut still = result(&format!("still-{n}"), Reviewer::Claude, "A");
        still.accepted = false;
        still.existing_findings.push(ExistingFinding {
            existing_finding_id: id.clone(),
            disposition: Disposition::StillOpen,
            evidence: artifact("still"),
        });
        let receipt = apply(&mut ledger, still);
        assert_eq!(
            progress
                .observe(&receipt.result.binding.review_id, &ledger, 2)
                .unwrap(),
            n == 2
        );
        assert_eq!(
            progress
                .observe(&receipt.result.binding.review_id, &ledger, 2)
                .unwrap(),
            n == 2
        );
    }
    assert_eq!(progress.rounds.len(), 2);
}

#[test]
fn duplicate_alias_plus_resolution_is_rejected_and_cannot_hide_new_blocker() {
    let (mut ledger, id) = with_finding();
    let mut r = result("ambiguous", Reviewer::Claude, "B");
    r.existing_findings.push(ExistingFinding {
        existing_finding_id: id.clone(),
        disposition: Disposition::VerifiedResolved,
        evidence: artifact("fix"),
    });
    let mut alias = new_finding();
    alias.duplicate_of = Some(id);
    alias.duplicate_reason = Some("same failure".into());
    r.new_findings.push(alias);
    ledger.allocate_review(r.binding.clone()).unwrap();
    let before = ledger.clone();
    assert!(ledger.apply_review(r).is_err());
    assert_eq!(ledger, before);
    let mut malformed = result("accept-with-blocker", Reviewer::Claude, "B");
    malformed.new_findings.push(new_finding());
    ledger.allocate_review(malformed.binding.clone()).unwrap();
    assert!(ledger.apply_review(malformed).is_err());
}
