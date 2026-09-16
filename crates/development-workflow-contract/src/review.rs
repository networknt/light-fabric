use crate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reviewer {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Blocking,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingState {
    Open,
    Disputed,
    Resolved,
    Waived,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub reviewer: Reviewer,
    pub repository: String,
    pub location: String,
    pub failure: String,
    pub resolution: String,
    pub evidence: ArtifactRef,
    pub severity: Severity,
    pub state: FindingState,
    pub history: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewFinding {
    pub local_id: String,
    pub repository: String,
    pub location: String,
    pub failure: String,
    pub resolution: String,
    pub evidence: ArtifactRef,
    pub severity: Severity,
    /// A single canonical ID from this feature's supplied ledger, not a local ID.
    pub duplicate_of: Option<String>,
    pub duplicate_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    StillOpen,
    VerifiedResolved,
    Reopened,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExistingFinding {
    pub existing_finding_id: String,
    pub disposition: Disposition,
    pub evidence: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewBinding {
    pub feature_run_id: String,
    pub review_id: String,
    pub stage_execution_id: String,
    pub reviewer: Reviewer,
    pub session_id: String,
    pub candidate: String,
    pub repositories: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewResult {
    pub binding: ReviewBinding,
    pub accepted: bool,
    pub evidence: ArtifactRef,
    pub existing_findings: Vec<ExistingFinding>,
    pub new_findings: Vec<NewFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewReceipt {
    pub request_digest: String,
    pub result: ReviewResult,
    pub canonical_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Remediation {
    pub turn_id: String,
    pub candidate: String,
    pub finding_id: String,
    pub changed_paths: BTreeSet<String>,
    pub evidence: ArtifactRef,
    pub dispute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HumanDisposition {
    pub decision_id: String,
    pub finding_id: String,
    pub deferred: bool,
    pub reason: String,
    pub tracking_reference: String,
    /// Produced by the authenticated human-decision boundary, not by a worker.
    pub authority: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FindingLedger {
    pub feature_run_id: String,
    pub findings: BTreeMap<String, Finding>,
    pub allocated_reviews: BTreeMap<String, ReviewBinding>,
    pub reviews: BTreeMap<String, ReviewReceipt>,
    pub remediations: BTreeMap<String, Remediation>,
    pub human_dispositions: BTreeMap<String, HumanDisposition>,
}
impl FindingLedger {
    pub fn new(feature_run_id: String) -> Self {
        Self {
            feature_run_id,
            findings: BTreeMap::new(),
            allocated_reviews: BTreeMap::new(),
            reviews: BTreeMap::new(),
            remediations: BTreeMap::new(),
            human_dispositions: BTreeMap::new(),
        }
    }

    /// Called by Workflow before dispatch. Worker output must match this binding.
    pub fn allocate_review(&mut self, binding: ReviewBinding) -> Result<()> {
        require(
            binding.feature_run_id == self.feature_run_id
                && !binding.review_id.is_empty()
                && !binding.stage_execution_id.is_empty()
                && !binding.session_id.is_empty()
                && digest_valid(&binding.candidate)
                && !binding.repositories.is_empty(),
            "invalid review allocation",
        )?;
        if let Some(old) = self.allocated_reviews.get(&binding.review_id) {
            return require(old == &binding, "review allocation conflict");
        }
        self.allocated_reviews
            .insert(binding.review_id.clone(), binding);
        Ok(())
    }

    /// Transactional in memory: rejected results leave no partial findings.
    pub fn apply_review(&mut self, result: ReviewResult) -> Result<ReviewReceipt> {
        require(
            self.allocated_reviews.get(&result.binding.review_id) == Some(&result.binding),
            "unallocated or changed review binding",
        )?;
        let request_digest = fingerprint(&result)?;
        if let Some(old) = self.reviews.get(&result.binding.review_id) {
            require(
                old.request_digest == request_digest,
                "review replay conflict",
            )?;
            return Ok(old.clone());
        }
        result.evidence.validate()?;
        let mut next = self.clone();
        let mut touched = BTreeSet::new();
        for item in &result.existing_findings {
            item.evidence.validate()?;
            require(
                touched.insert(item.existing_finding_id.clone()),
                "conflicting finding dispositions",
            )?;
            let f = next
                .findings
                .get_mut(&item.existing_finding_id)
                .ok_or(ContractError("unknown canonical finding"))?;
            require(
                result.binding.repositories.contains(&f.repository)
                    && f.reviewer == result.binding.reviewer,
                "finding outside reviewer scope",
            )?;
            f.state = match item.disposition {
                Disposition::StillOpen => {
                    require(
                        matches!(f.state, FindingState::Open | FindingState::Disputed),
                        "closed finding must explicitly reopen",
                    )?;
                    f.state
                }
                Disposition::VerifiedResolved => FindingState::Resolved,
                Disposition::Reopened => {
                    require(
                        matches!(
                            f.state,
                            FindingState::Resolved | FindingState::Waived | FindingState::Deferred
                        ),
                        "only closed findings reopen",
                    )?;
                    FindingState::Open
                }
            };
            f.history.push(format!(
                "review:{}:{:?}:{}",
                result.binding.review_id, item.disposition, item.evidence.digest
            ));
        }
        let mut canonical_ids = BTreeMap::new();
        for item in &result.new_findings {
            require(
                !item.local_id.trim().is_empty()
                    && !item.location.trim().is_empty()
                    && !item.failure.trim().is_empty()
                    && !item.resolution.trim().is_empty()
                    && result.binding.repositories.contains(&item.repository),
                "invalid new finding",
            )?;
            require(
                !canonical_ids.contains_key(&item.local_id),
                "duplicate local finding ID",
            )?;
            item.evidence.validate()?;
            let id = if let Some(existing) = &item.duplicate_of {
                require(
                    item.duplicate_reason
                        .as_ref()
                        .is_some_and(|r| !r.trim().is_empty()),
                    "duplicate explanation required",
                )?;
                // Resolve against the pre-turn ledger: local aliases/chains are ambiguous.
                let old = self
                    .findings
                    .get(existing)
                    .ok_or(ContractError("unknown duplicate reference"))?;
                require(
                    old.repository == item.repository
                        && old.reviewer == result.binding.reviewer
                        && matches!(old.state, FindingState::Open | FindingState::Disputed)
                        && !touched.contains(existing),
                    "ambiguous duplicate reference",
                )?;
                next.findings
                    .get_mut(existing)
                    .unwrap()
                    .history
                    .push(format!(
                        "alias:{}:{}:{}",
                        result.binding.review_id,
                        item.local_id,
                        item.duplicate_reason.as_ref().unwrap()
                    ));
                existing.clone()
            } else {
                require(
                    item.duplicate_reason.is_none(),
                    "duplicate reason without reference",
                )?;
                let id = identity(
                    "finding/v1",
                    &[
                        &self.feature_run_id,
                        &result.binding.review_id,
                        &item.local_id,
                    ],
                );
                require(
                    !next.findings.contains_key(&id),
                    "finding identity collision",
                )?;
                next.findings.insert(
                    id.clone(),
                    Finding {
                        id: id.clone(),
                        reviewer: result.binding.reviewer,
                        repository: item.repository.clone(),
                        location: item.location.clone(),
                        failure: item.failure.clone(),
                        resolution: item.resolution.clone(),
                        evidence: item.evidence.clone(),
                        severity: item.severity,
                        state: FindingState::Open,
                        history: vec![format!(
                            "created:{}:{}",
                            result.binding.review_id, item.local_id
                        )],
                    },
                );
                id
            };
            canonical_ids.insert(item.local_id.clone(), id);
        }
        require(
            !result.accepted || next.closed(),
            "accepted verdict has unresolved findings",
        )?;
        let receipt = ReviewReceipt {
            request_digest,
            result,
            canonical_ids,
        };
        next.reviews
            .insert(receipt.result.binding.review_id.clone(), receipt.clone());
        *self = next;
        Ok(receipt)
    }

    /// An implementer can record evidence/dispute, never mark a finding closed.
    pub fn remediate(&mut self, item: Remediation) -> Result<()> {
        require(
            !item.turn_id.is_empty() && digest_valid(&item.candidate),
            "invalid remediation",
        )?;
        item.evidence.validate()?;
        let key = identity(
            "remediation/v1",
            &[&self.feature_run_id, &item.turn_id, &item.finding_id],
        );
        if let Some(old) = self.remediations.get(&key) {
            return require(old == &item, "remediation replay conflict");
        }
        let f = self
            .findings
            .get_mut(&item.finding_id)
            .ok_or(ContractError("unknown remediation finding"))?;
        require(
            matches!(f.state, FindingState::Open | FindingState::Disputed),
            "remediation requires open finding",
        )?;
        require(
            item.dispute || !item.changed_paths.is_empty(),
            "fix has no changed paths",
        )?;
        if item.dispute {
            f.state = FindingState::Disputed;
        }
        f.history.push(format!(
            "remediation:{}:{}",
            item.turn_id, item.evidence.digest
        ));
        self.remediations.insert(key, item);
        Ok(())
    }

    /// The caller must authenticate the human and verify this authority receipt.
    pub fn dispose_by_human(&mut self, item: HumanDisposition) -> Result<()> {
        require(
            !item.decision_id.is_empty()
                && !item.reason.trim().is_empty()
                && !item.tracking_reference.trim().is_empty(),
            "human disposition needs reason and tracking",
        )?;
        item.authority.validate()?;
        if let Some(old) = self.human_dispositions.get(&item.decision_id) {
            return require(old == &item, "human disposition conflict");
        }
        let f = self
            .findings
            .get_mut(&item.finding_id)
            .ok_or(ContractError("unknown human finding"))?;
        require(
            matches!(f.state, FindingState::Open | FindingState::Disputed),
            "finding already closed",
        )?;
        f.state = if item.deferred {
            FindingState::Deferred
        } else {
            FindingState::Waived
        };
        f.history.push(format!(
            "human:{}:{}",
            item.decision_id, item.authority.digest
        ));
        self.human_dispositions
            .insert(item.decision_id.clone(), item);
        Ok(())
    }

    pub fn closed(&self) -> bool {
        self.findings.values().all(|f| {
            f.severity == Severity::Advisory
                || !matches!(f.state, FindingState::Open | FindingState::Disputed)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewVerdict {
    pub reviewer: Reviewer,
    pub review_id: String,
    pub session_id: String,
    pub candidate: String,
    pub accepted: bool,
    pub evidence: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoverageTransition {
    pub delta: DeltaReceipt,
    pub validation: ValidationReceipt,
    pub active_reviewer: Reviewer,
    pub reviews: Vec<ReviewVerdict>,
    pub closed_finding_ids: BTreeSet<String>,
    /// Explicit unaffected-scope carry-forward; disallowed for broad changes.
    pub carried_reviewers: BTreeSet<Reviewer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewCoverage {
    pub baseline: String,
    pub snapshots: BTreeMap<String, CandidateSnapshot>,
    /// Original sequential Codex then Claude full reviews; findings are allowed.
    pub full_reviews: Vec<ReviewVerdict>,
    pub transitions: Vec<CoverageTransition>,
}
impl ReviewCoverage {
    pub fn validate_publication(
        &self,
        current: &str,
        ledger: &FindingLedger,
        checks: &ValidationReceipt,
    ) -> Result<()> {
        require(ledger.closed(), "unresolved finding blocks publication")?;
        require(
            self.full_reviews.len() == 2
                && self.full_reviews[0].reviewer == Reviewer::Codex
                && self.full_reviews[1].reviewer == Reviewer::Claude,
            "sequential full reviews required",
        )?;
        let mut sessions = BTreeMap::new();
        let mut accepted = BTreeMap::new();
        let mut review_ids = BTreeSet::new();
        require(
            self.full_reviews[0].candidate == self.baseline,
            "full review baseline mismatch",
        )?;
        let mut full_index = 0;
        self.snapshot(&self.baseline, &ledger.feature_run_id)?;
        let mut head = self.baseline.as_str();
        let mut visited = BTreeSet::from([head]);
        // Full reviews occur once in sequence, but Codex can fix/verify before
        // Claude starts its first full review of the resulting candidate.
        for transition in self
            .transitions
            .iter()
            .map(Some)
            .chain(std::iter::once(None))
        {
            while full_index < self.full_reviews.len()
                && self.full_reviews[full_index].candidate == head
            {
                let review = &self.full_reviews[full_index];
                require(
                    full_index == 0 || accepted.get(&Reviewer::Codex) == Some(&true),
                    "Claude full review precedes Codex closure",
                )?;
                verify_verdict(review, ledger)?;
                self.review_scope(review, ledger)?;
                require(
                    review_ids.insert(review.review_id.clone()),
                    "review ID reused",
                )?;
                sessions.insert(review.reviewer, review.session_id.clone());
                accepted.insert(review.reviewer, review.accepted);
                full_index += 1;
            }
            let Some(transition) = transition else {
                break;
            };
            let delta = &transition.delta;
            require(
                delta.before == head && delta.before != delta.after && visited.insert(&delta.after),
                "missing, repeated or forked coverage transition",
            )?;
            self.snapshot(&delta.after, &ledger.feature_run_id)?;
            delta.full_delta.validate()?;
            require(
                !delta.changed_paths.is_empty(),
                "missing complete delta paths",
            )?;
            transition.validation.validate(&delta.after)?;
            require(
                transition.validation.required_checks == checks.required_checks,
                "coverage check policy changed",
            )?;
            let mut reviewed = BTreeSet::new();
            for review in &transition.reviews {
                verify_verdict(review, ledger)?;
                self.review_scope(review, ledger)?;
                require(
                    review.candidate == delta.after
                        && review.accepted
                        && sessions.get(&review.reviewer) == Some(&review.session_id),
                    "fix review must resume its bound reviewer",
                )?;
                require(
                    reviewed.insert(review.reviewer) && review_ids.insert(review.review_id.clone()),
                    "duplicate transition review",
                )?;
            }
            require(
                reviewed.contains(&transition.active_reviewer),
                "active fix reviewer has not accepted",
            )?;
            require(
                reviewed.is_disjoint(&transition.carried_reviewers),
                "review cannot also be carried",
            )?;
            for role in sessions.keys().copied() {
                if !reviewed.contains(&role) {
                    require(
                        !delta.broad()
                            && transition.carried_reviewers.contains(&role)
                            && accepted.get(&role) == Some(&true),
                        "missing required broader review or carry-forward",
                    )?;
                }
                accepted.insert(role, true);
            }
            for id in &transition.closed_finding_ids {
                let f = ledger
                    .findings
                    .get(id)
                    .ok_or(ContractError("unknown closure finding"))?;
                require(
                    !matches!(f.state, FindingState::Open | FindingState::Disputed),
                    "finding closure not verified",
                )?;
                require(
                    transition.reviews.iter().any(|review| {
                        ledger.reviews[&review.review_id]
                            .result
                            .existing_findings
                            .iter()
                            .any(|item| {
                                item.existing_finding_id == *id
                                    && item.disposition == Disposition::VerifiedResolved
                            })
                    }),
                    "closure lacks transition reviewer evidence",
                )?;
            }
            head = &delta.after;
        }
        require(
            full_index == 2 && head == current && accepted.values().all(|a| *a),
            "current candidate lacks accepted coverage",
        )?;
        checks.validate(current)
    }
    fn snapshot(&self, digest: &str, feature: &str) -> Result<()> {
        let snapshot = self
            .snapshots
            .get(digest)
            .ok_or(ContractError("missing durable snapshot evidence"))?;
        snapshot.validate()?;
        require(
            snapshot.candidate_digest == digest && snapshot.feature_run_id == feature,
            "snapshot binding mismatch",
        )
    }

    fn review_scope(&self, review: &ReviewVerdict, ledger: &FindingLedger) -> Result<()> {
        let snapshot = self
            .snapshots
            .get(&review.candidate)
            .ok_or(ContractError("missing reviewed snapshot"))?;
        let baseline = &self.snapshots[&self.baseline];
        let binding = &ledger.reviews[&review.review_id].result.binding;
        require(
            binding.stage_execution_id == snapshot.stage_execution_id
                && snapshot.stage_execution_id == baseline.stage_execution_id
                && snapshot.task_id == baseline.task_id
                && snapshot
                    .repositories
                    .keys()
                    .all(|repo| binding.repositories.contains(repo)),
            "review does not cover candidate scope",
        )
    }
}
fn verify_verdict(review: &ReviewVerdict, ledger: &FindingLedger) -> Result<()> {
    review.evidence.validate()?;
    let saved = &ledger
        .reviews
        .get(&review.review_id)
        .ok_or(ContractError("review has no persisted result"))?
        .result;
    require(
        saved.binding.reviewer == review.reviewer
            && saved.binding.session_id == review.session_id
            && saved.binding.candidate == review.candidate
            && saved.accepted == review.accepted
            && saved.evidence == review.evidence,
        "verdict differs from immutable review result",
    )
}
