use crate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StageKind {
    Intake,
    Design,
    Plan,
    Implement,
    Finalize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StageSelector {
    pub kind: StageKind,
    pub phase_id: Option<String>,
}
impl StageSelector {
    pub fn validate(&self) -> Result<()> {
        require(
            if self.kind == StageKind::Implement {
                self.phase_id.as_ref().is_some_and(|v| !v.is_empty())
            } else {
                self.phase_id.is_none()
            },
            "invalid stage phase",
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionTarget {
    PrReady,
    Merged,
    Deployed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RequirementArtifact {
    pub version: u64,
    pub content: ArtifactRef,
    pub scope: Vec<String>,
    pub non_goals: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub repositories: BTreeSet<String>,
    pub ownership_unknown: bool,
    pub compatibility_concerns: Vec<String>,
    pub sources: Vec<ArtifactRef>,
    pub unresolved_decisions: Vec<String>,
    pub completion_target: CompletionTarget,
    pub completion_checks: BTreeSet<String>,
}
impl RequirementArtifact {
    pub fn validate_accepted(&self) -> Result<()> {
        self.content.validate()?;
        require(
            self.version > 0
                && !self.scope.is_empty()
                && !self.acceptance_criteria.is_empty()
                && (!self.repositories.is_empty() || self.ownership_unknown)
                && !self.completion_checks.is_empty()
                && self.unresolved_decisions.is_empty(),
            "requirements incomplete",
        )?;
        for source in &self.sources {
            source.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PhaseDefinition {
    pub phase_id: String,
    pub scope: ArtifactRef,
    pub repositories: BTreeSet<String>,
    pub dependencies: BTreeSet<String>,
    pub required_checks: BTreeSet<String>,
    pub exit_criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StageClaim {
    pub feature_run_id: String,
    /// Store-owned transition identity, renewed only by explicit reopen/replan.
    pub transition_id: String,
    pub predecessor_version: u64,
    pub stage: StageSelector,
    pub inputs: BTreeMap<String, ArtifactRef>,
    pub definition: ArtifactRef,
    pub workspace_binding: ArtifactRef,
    pub deadline_epoch_seconds: u64,
}
impl StageClaim {
    pub fn identity(&self) -> Result<String> {
        self.stage.validate()?;
        Ok(identity(
            "stage-claim/v1",
            &[
                &self.feature_run_id,
                &self.transition_id,
                &self.predecessor_version.to_string(),
                &fingerprint(&self.stage)?,
            ],
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StageClaimReceipt {
    pub claim_id: String,
    pub request_digest: String,
    pub stage_execution_id: String,
    pub workflow_instance_id: String,
    pub process_id: String,
    pub feature_version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureState {
    ReadyForNextStage,
    Active,
    ReplanRequired,
    HumanResolutionRequired,
    ReauthorizationRequired,
    VmReleasePending,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FeatureRun {
    pub schema_version: u32,
    pub feature_run_id: String,
    pub issue: IssueRef,
    pub version: u64,
    pub state: FeatureState,
    pub transition_id: String,
    pub allowed_stage: StageSelector,
    pub accepted_inputs: BTreeMap<String, ArtifactRef>,
    pub active_claim: Option<StageClaimReceipt>,
    pub claims: BTreeMap<String, StageClaimReceipt>,
    pub accepted_results: Vec<StageResult>,
    pub budgets: BudgetLedger,
    pub vm: VmReservation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IssueRef {
    pub repository: String,
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDecision {
    Replay(StageClaimReceipt),
    Admit {
        claim_id: String,
        request_digest: String,
    },
}
impl FeatureRun {
    /// Decision only. Phase 1 must lock the feature and persist the claim plus
    /// process/initial task in the same accept_invocation transaction.
    pub fn check_claim(&self, claim: &StageClaim, now: u64) -> Result<ClaimDecision> {
        require(
            self.schema_version == 1 && claim.feature_run_id == self.feature_run_id,
            "feature identity/version mismatch",
        )?;
        let claim_id = claim.identity()?;
        let request_digest = fingerprint(claim)?;
        if let Some(old) = self.claims.get(&claim_id) {
            require(old.request_digest == request_digest, "claim input conflict")?;
            return Ok(ClaimDecision::Replay(old.clone()));
        }
        require(
            self.state == FeatureState::ReadyForNextStage && self.active_claim.is_none(),
            "feature already owned or blocked",
        )?;
        require(
            self.vm.feature_run_id == self.feature_run_id
                && !self.vm.released
                && !self.vm.release_pending,
            "VM slot unavailable",
        )?;
        require(
            claim.predecessor_version == self.version
                && claim.transition_id == self.transition_id
                && claim.stage == self.allowed_stage
                && claim.inputs == self.accepted_inputs,
            "stale or disallowed stage handoff",
        )?;
        require(
            now < claim.deadline_epoch_seconds
                && claim.deadline_epoch_seconds <= self.budgets.deadline_epoch_seconds,
            "stage deadline widens or expired",
        )?;
        claim.definition.validate()?;
        claim.workspace_binding.validate()?;
        for input in claim.inputs.values() {
            input.validate()?;
        }
        Ok(ClaimDecision::Admit {
            claim_id,
            request_digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StageResult {
    pub feature_run_id: String,
    pub claim: StageClaimReceipt,
    pub stage: StageSelector,
    pub inputs: BTreeMap<String, ArtifactRef>,
    pub outputs: BTreeMap<String, ArtifactRef>,
    pub candidate: CandidateSnapshot,
    pub validation: ValidationReceipt,
    pub review_ids: BTreeSet<String>,
    pub finding_ids: BTreeSet<String>,
    pub native_checkpoint: Option<ArtifactRef>,
    pub publication_receipts: Vec<PublicationReceipt>,
    pub accepted: bool,
}
impl StageResult {
    pub fn validate_handoff(&self, feature: &FeatureRun) -> Result<()> {
        require(
            self.accepted
                && self.feature_run_id == feature.feature_run_id
                && self.inputs == feature.accepted_inputs
                && feature.active_claim.as_ref() == Some(&self.claim)
                && self.stage == feature.allowed_stage,
            "stale stage result",
        )?;
        self.candidate.validate()?;
        require(
            self.candidate.feature_run_id == self.feature_run_id
                && self.candidate.stage_execution_id == self.claim.stage_execution_id,
            "snapshot belongs to another stage",
        )?;
        self.validation.validate(&self.candidate.candidate_digest)?;
        require(
            !self.outputs.is_empty()
                && (self.stage.kind == StageKind::Finalize || !self.review_ids.is_empty()),
            "stage result lacks output or review evidence",
        )?;
        for artifact in self.outputs.values() {
            artifact.validate()?;
        }
        if let Some(checkpoint) = &self.native_checkpoint {
            checkpoint.validate()?;
        }
        Ok(())
    }

    /// Policy and ledger are loaded by Workflow, never selected by worker output.
    pub fn validate_acceptance(
        &self,
        feature: &FeatureRun,
        policy: &StageAcceptancePolicy,
        ledger: &FindingLedger,
        signoff: Option<&DesignSignoff>,
    ) -> Result<()> {
        self.validate_handoff(feature)?;
        require(
            ledger.feature_run_id == self.feature_run_id && ledger.closed(),
            "stage has unresolved findings",
        )?;
        require(
            (!policy.required_reviewers.is_empty()
                || (self.stage.kind == StageKind::Finalize
                    && feature.accepted_results.last().is_some_and(|source| {
                        source.accepted
                            && source.stage.kind == StageKind::Design
                            && !source.review_ids.is_empty()
                    })
                    && feature.accepted_results.iter().all(|source| {
                        source.accepted
                            && matches!(source.stage.kind, StageKind::Intake | StageKind::Design)
                    })))
                && !policy.required_checks.is_empty()
                && policy.required_checks == self.validation.required_checks,
            "stage validation policy mismatch",
        )?;
        let mut roles = BTreeSet::new();
        for id in &self.review_ids {
            let result = &ledger
                .reviews
                .get(id)
                .ok_or(ContractError("missing stage review result"))?
                .result;
            require(
                result.accepted
                    && result.binding.candidate == self.candidate.candidate_digest
                    && result.binding.stage_execution_id == self.claim.stage_execution_id
                    && self
                        .candidate
                        .repositories
                        .keys()
                        .all(|repo| result.binding.repositories.contains(repo)),
                "stage review binding mismatch",
            )?;
            roles.insert(result.binding.reviewer);
        }
        require(
            policy.required_reviewers.is_subset(&roles),
            "required stage reviewer missing",
        )?;
        require(
            self.finding_ids
                .iter()
                .all(|id| ledger.findings.contains_key(id)),
            "unknown stage finding",
        )?;
        if policy.require_design_signoff {
            let design = self
                .outputs
                .get("design")
                .or_else(|| self.inputs.get("design"))
                .ok_or(ContractError("missing accepted design"))?;
            validate_signoff(true, &self.feature_run_id, &design.digest, signoff)?;
        }
        let mut intents = BTreeSet::new();
        for receipt in &self.publication_receipts {
            receipt.validate()?;
            require(
                receipt.candidate_digest == self.candidate.candidate_digest
                    && intents.insert(receipt.intent_id.clone()),
                "publication candidate/identity mismatch",
            )?;
        }
        require(
            policy.required_publication_intents.is_subset(&intents),
            "required publication incomplete",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StageAcceptancePolicy {
    pub required_reviewers: BTreeSet<Reviewer>,
    pub required_checks: BTreeSet<String>,
    pub require_design_signoff: bool,
    pub required_publication_intents: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnKind {
    Author,
    Review,
    Remediation,
    ValidationFix,
    OutputRepair,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnCharge {
    pub logical_turn_id: String,
    pub stage_execution_id: String,
    /// Stable scope (including final remediation) survives stage replacement.
    pub budget_scope: String,
    pub kind: TurnKind,
    /// Only a new dispatched remediation round consumes a round; repairs still
    /// consume turns. Retried dispatch retains both identities.
    pub remediation_round_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BudgetLedger {
    pub maximum_turns: u32,
    pub maximum_remediation_rounds: u32,
    pub deadline_epoch_seconds: u64,
    pub charges: BTreeMap<String, TurnCharge>,
}
impl BudgetLedger {
    pub fn new(maximum_turns: u32, deadline_epoch_seconds: u64) -> Self {
        Self {
            maximum_turns,
            maximum_remediation_rounds: 3,
            deadline_epoch_seconds,
            charges: BTreeMap::new(),
        }
    }
    pub fn charge(&mut self, charge: TurnCharge, now: u64) -> Result<()> {
        require(
            !charge.logical_turn_id.is_empty()
                && !charge.stage_execution_id.is_empty()
                && !charge.budget_scope.is_empty(),
            "invalid logical turn",
        )?;
        if let Some(old) = self.charges.get(&charge.logical_turn_id) {
            return require(old == &charge, "logical turn replay conflict");
        }
        require(
            now < self.deadline_epoch_seconds && self.charges.len() < self.maximum_turns as usize,
            "human resolution required: turn/deadline budget exhausted",
        )?;
        require(
            charge.kind != TurnKind::Remediation
                || charge
                    .remediation_round_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()),
            "remediation requires round identity",
        )?;
        if let Some(round) = &charge.remediation_round_id {
            require(!round.is_empty(), "empty remediation round")?;
            let rounds: BTreeSet<_> = self
                .charges
                .values()
                .filter(|c| c.budget_scope == charge.budget_scope)
                .filter_map(|c| c.remediation_round_id.as_ref())
                .collect();
            require(
                rounds.contains(round) || rounds.len() < self.maximum_remediation_rounds as usize,
                "human resolution required: remediation budget exhausted",
            )?;
        }
        self.charges.insert(charge.logical_turn_id.clone(), charge);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignoffDecision {
    Approved,
    ChangesRequested,
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DesignSignoff {
    pub feature_run_id: String,
    pub design_digest: String,
    pub decision: SignoffDecision,
    pub authority: ArtifactRef,
}
pub fn validate_signoff(
    required: bool,
    feature: &str,
    design: &str,
    signoff: Option<&DesignSignoff>,
) -> Result<()> {
    require(digest_valid(design), "invalid design digest")?;
    if !required {
        return Ok(());
    }
    let decision = signoff.ok_or(ContractError("design sign-off pending"))?;
    decision.authority.validate()?;
    require(
        decision.feature_run_id == feature && decision.design_digest == design,
        "stale design sign-off",
    )?;
    require(
        decision.decision == SignoffDecision::Approved,
        "design sign-off not approved",
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VmReservation {
    pub vm_id: String,
    pub runner_binding: ArtifactRef,
    pub feature_run_id: String,
    pub generation: u64,
    pub acquired_epoch_seconds: u64,
    pub release_pending: bool,
    pub released: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VmReleaseReceipt {
    pub vm_id: String,
    pub feature_run_id: String,
    pub generation: u64,
    pub terminal_state: FeatureState,
    pub dispatch_fenced: ArtifactRef,
    pub execution_and_effect_receipts: BTreeMap<String, ArtifactRef>,
}
impl VmReservation {
    pub fn validate_release(
        &self,
        receipt: &VmReleaseReceipt,
        outstanding: &BTreeSet<String>,
    ) -> Result<()> {
        require(
            receipt.vm_id == self.vm_id
                && receipt.feature_run_id == self.feature_run_id
                && receipt.generation == self.generation,
            "stale VM release",
        )?;
        require(
            matches!(
                receipt.terminal_state,
                FeatureState::Completed | FeatureState::Cancelled | FeatureState::Failed
            ),
            "nonterminal feature retains VM",
        )?;
        receipt.dispatch_fenced.validate()?;
        require(
            outstanding
                .iter()
                .all(|id| receipt.execution_and_effect_receipts.contains_key(id)),
            "VM release pending execution/effect fencing",
        )?;
        for evidence in receipt.execution_and_effect_receipts.values() {
            evidence.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DocumentRevision {
    pub feature_run_id: String,
    pub document_id: String,
    pub revision: u64,
    pub accepted_content: ArtifactRef,
    pub supersedes: Option<ArtifactRef>,
    pub publication_task_id: String,
}
impl DocumentRevision {
    pub fn task_identity(&self) -> String {
        identity(
            "document-publication/v1",
            &[
                &self.feature_run_id,
                &self.document_id,
                &self.revision.to_string(),
            ],
        )
    }
    pub fn validate(&self) -> Result<()> {
        self.accepted_content.validate()?;
        require(
            !self.feature_run_id.is_empty()
                && !self.document_id.is_empty()
                && self.revision > 0
                && self.publication_task_id == self.task_identity(),
            "invalid document publication identity",
        )?;
        if let Some(old) = &self.supersedes {
            old.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PublicationReceipt {
    pub intent_id: String,
    pub repository: String,
    pub candidate_digest: String,
    pub task_id: String,
    pub commit: String,
    pub parent_commit: String,
    pub remote_ref: String,
    pub pull_request_number: u64,
    pub pull_request_url: String,
    pub target_branch: String,
    pub permalink: String,
    pub verification: ArtifactRef,
}
impl PublicationReceipt {
    pub fn validate(&self) -> Result<()> {
        self.verification.validate()?;
        require(
            !self.intent_id.is_empty()
                && !self.repository.is_empty()
                && !self.task_id.is_empty()
                && digest_valid(&self.candidate_digest)
                && git_oid(&self.commit)
                && git_oid(&self.parent_commit)
                && self.remote_ref.starts_with("refs/heads/")
                && self.remote_ref != "refs/heads/develop"
                && self.remote_ref != "refs/heads/master"
                && self.remote_ref != "refs/heads/main"
                && self.pull_request_number > 0
                && self.pull_request_url.starts_with("https://")
                && self.target_branch == "develop"
                && self.permalink.contains(&self.commit),
            "invalid publication receipt",
        )
    }
}

/// Repeated still-open IDs are deterministic non-progress evidence. This is a
/// routing signal for human resolution, never a substitute for reviewer semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NonProgress {
    pub rounds: BTreeMap<String, BTreeSet<String>>,
    pub counts: BTreeMap<String, usize>,
    pub decisions: BTreeMap<String, bool>,
}
impl NonProgress {
    pub fn observe(
        &mut self,
        review_id: &str,
        ledger: &FindingLedger,
        limit: usize,
    ) -> Result<bool> {
        require(limit > 0, "non-progress threshold must be positive")?;
        if let Some(decision) = self.decisions.get(review_id) {
            return Ok(*decision);
        }
        let result = &ledger
            .reviews
            .get(review_id)
            .ok_or(ContractError("unknown non-progress review"))?
            .result;
        let ids: BTreeSet<_> = result
            .existing_findings
            .iter()
            .filter(|f| f.disposition == Disposition::StillOpen)
            .map(|f| f.existing_finding_id.clone())
            .collect();
        for item in &result.existing_findings {
            if item.disposition != Disposition::StillOpen {
                self.counts.remove(&item.existing_finding_id);
            }
        }
        for id in &ids {
            *self.counts.entry(id.clone()).or_default() += 1;
        }
        self.rounds.insert(review_id.into(), ids.clone());
        let decision = ids.iter().any(|id| self.counts[id] >= limit);
        self.decisions.insert(review_id.into(), decision);
        Ok(decision)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CommentPurpose {
    Status,
    Round {
        stage_execution_id: String,
        round: u32,
    },
    Publication {
        artifact_id: String,
        revision: u64,
        repository: String,
        action: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommentIntent {
    pub feature_run_id: String,
    pub purpose: CommentPurpose,
    pub version: u64,
    pub body: ArtifactRef,
}
impl CommentIntent {
    pub fn identity(&self) -> Result<String> {
        require(
            !self.feature_run_id.is_empty() && self.version > 0,
            "invalid comment intent",
        )?;
        self.body.validate()?;
        Ok(identity(
            "comment/v1",
            &[&self.feature_run_id, &fingerprint(&self.purpose)?],
        ))
    }
    pub fn check_update(&self, previous: &Self) -> Result<()> {
        require(
            self.identity()? == previous.identity()?,
            "comment identity mismatch",
        )?;
        if self == previous {
            return Ok(());
        }
        require(
            matches!(self.purpose, CommentPurpose::Status) && self.version > previous.version,
            "stale or immutable comment update",
        )
    }
}
