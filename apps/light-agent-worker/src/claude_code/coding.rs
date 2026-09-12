//! Coding integration for the locally qualified candidate. Caller supplies trusted
//! host paths and a policy; nativeModel is the only caller-selectable model field.
//! Production admission and distribution remain gated independently.
use super::*;
use agent_materializer::MaterializationManifest;
use coding_agent_runtime::{
    CodingAuthenticationEvidence, CodingCredentialSource, CodingReviewResult, CodingRole,
    patch_digest, validate_patch,
};
use execution_security::ProtectedPathPolicy;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingOutput {
    pub adapter: &'static str,
    pub authentication: Option<CodingAuthenticationEvidence>,
    pub coding_thread: Value,
    pub patch: Option<Value>,
    pub coding_review: Option<CodingReviewResult>,
    pub final_message: Option<String>,
    pub native_model: Option<String>,
    pub advisory_usage: Option<Value>,
}

/// Runner-owned immutable bundle path, not a path taken from the model's prompt.
/// Its bytes must match the admitted repository digest. Each invocation uses a
/// fresh workspace, including reviewer resume turns.
pub async fn execute_coding(
    host: &HostContext,
    bundle: &Path,
    manifest: &MaterializationManifest,
    turn: ClaudeTurn,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: mpsc::Sender<Event>,
) -> Result<CodingOutput> {
    execute_coding_inner(
        host,
        bundle,
        manifest,
        turn,
        cancel,
        deadline,
        events,
        BINARY_SHA256,
    )
    .await
}

async fn execute_coding_inner(
    host: &HostContext,
    bundle: &Path,
    manifest: &MaterializationManifest,
    turn: ClaudeTurn,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: mpsc::Sender<Event>,
    binary_digest: &str,
) -> Result<CodingOutput> {
    turn.coding.validate()?;
    ensure!(
        manifest.schema_version == 1
            && manifest.product_profile == agent_materializer::ProductProfile::Coding
            && manifest.packages.is_empty()
            && manifest.effective_instructions.is_empty(),
        "Claude Phase 2 requires an instruction-free coding manifest without packages"
    );
    ensure!(
        manifest.digest()? == turn.coding.materialization_manifest_digest
            && manifest.writable_roots == turn.coding.writable_roots,
        "Claude materialization manifest differs from the admitted turn"
    );
    let mut workspace = Workspace::new(bundle, &turn.coding, cancel.clone(), deadline).await?;
    let native_home = host.native_home.canonicalize()?;
    ensure!(
        !workspace.root.path().starts_with(&native_home),
        "native home overlaps coding workspace"
    );
    // Private Light checkpoints must not be writable by the native CLI, even
    // when the owner opts into native bypass permissions. Keep them outside its
    // writable configuration directory; only the trusted parent process writes.
    let key = hex::encode(Sha256::digest(native_home.as_os_str().as_encoded_bytes()));
    let checkpoint_home = native_home
        .parent()
        .context("native home has no parent")?
        .join(format!(".light-claude-checkpoints-{key}"));
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&checkpoint_home)
    {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let metadata = std::fs::symlink_metadata(&checkpoint_home)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "unsafe private Claude checkpoint directory"
    );
    workspace.checkpoint_home = checkpoint_home;
    let local_host = HostContext {
        executable: host.executable.clone(),
        native_home,
        working_directory: if turn.coding.role == CodingRole::Review {
            workspace.scratch.clone()
        } else {
            workspace.repository.clone()
        },
        thread_scope: host.thread_scope.clone(),
    };
    let spec = turn.coding.clone();
    let outcome = execute_prepared(
        &local_host,
        turn,
        cancel.clone(),
        deadline,
        events,
        binary_digest,
        Some(&workspace),
    )
    .await?;
    let proposal = match outcome {
        Outcome::Closed(receipt) => {
            return Ok(CodingOutput {
                adapter: ADAPTER_ID,
                authentication: None,
                coding_thread: receipt,
                patch: None,
                coding_review: None,
                final_message: None,
                native_model: None,
                advisory_usage: None,
            });
        }
        Outcome::Proposal(proposal) => proposal,
    };
    ensure!(
        !*cancel.borrow() && Instant::now() < deadline,
        "Claude artifact collection cancelled or expired"
    );
    ensure!(
        proposal.result.permission_denials == 0,
        "Claude permission denial prevents artifact acceptance"
    );
    let patch = workspace.diff().await?;
    let (artifact, review, checkpoint_patch) = if spec.role == CodingRole::Review {
        let input = spec.review_input.as_ref().context("missing review input")?;
        ensure!(
            patch == input.candidate_patch,
            "reviewer candidate was mutated"
        );
        let result: CodingReviewResult = serde_json::from_value(
            proposal
                .result
                .structured_output
                .clone()
                .context("reviewer omitted required structured output")?,
        )
        .map_err(|_| anyhow::anyhow!("reviewer result is not valid structured JSON"))?;
        result.validate()?;
        ensure!(
            result.review_id == input.review_id
                && result.artifact_digest == input.implementation.patch_digest,
            "review result differs from the admitted candidate"
        );
        (None, Some(result), "")
    } else {
        let paths = workspace
            .git(&["diff", "--name-only", "HEAD", "--"], b"")
            .await?;
        let paths = paths.lines().map(str::to_owned).collect::<Vec<_>>();
        let validated = validate_patch(
            &spec,
            &ProtectedPathPolicy::default_deny(),
            &spec.base_revision,
            &patch,
            &paths,
        )?;
        (Some(serde_json::to_value(validated)?), None, patch.as_str())
    };
    let authentication = CodingAuthenticationEvidence {
        profile: CodingAuthenticationProfile::PersonalSubscription,
        credential_source: CodingCredentialSource::NativeClaudeStore,
        credential_generation: None,
        authoritative_usage: false,
    };
    authentication.validate()?;
    let native = proposal.result.clone();
    ensure!(
        !*cancel.borrow() && Instant::now() < deadline,
        "Claude artifact acceptance cancelled or expired"
    );
    let receipt = proposal.accept_validated(checkpoint_patch)?;
    Ok(CodingOutput {
        adapter: ADAPTER_ID,
        authentication: Some(authentication),
        coding_thread: receipt,
        patch: artifact,
        coding_review: review,
        final_message: Some(native.text),
        native_model: Some(native.model),
        advisory_usage: native.usage,
    })
}

pub(super) struct Workspace {
    root: tempfile::TempDir,
    pub(super) checkpoint_home: PathBuf,
    repository: PathBuf,
    metadata: PathBuf,
    scratch: PathBuf,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
}
impl Workspace {
    async fn new(
        bundle: &Path,
        spec: &CodingTurnSpec,
        cancel: watch::Receiver<bool>,
        deadline: Instant,
    ) -> Result<Self> {
        let closing = spec
            .thread
            .as_ref()
            .is_some_and(|t| t.mode == CodingThreadMode::Close);
        ensure!(
            closing || bundle.is_absolute() && !bundle.is_symlink(),
            "bundle must be an absolute regular file"
        );
        let root = tempfile::tempdir()?;
        let repository = root.path().join("repository");
        let metadata = root.path().join("metadata");
        let scratch = root.path().join("scratch");
        tokio::fs::create_dir(&repository).await?;
        tokio::fs::create_dir(&scratch).await?;
        if closing {
            return Ok(Self {
                root,
                checkpoint_home: PathBuf::new(),
                repository,
                metadata,
                scratch,
                cancel,
                deadline,
            });
        }
        // Copy once and hash exactly the snapshot consumed by Git, avoiding a
        // hash-then-open race on the runner's staged input. Bound disk use as well.
        let snapshot = root.path().join("repository.bundle");
        let mut file = tokio::fs::File::open(bundle).await?;
        ensure!(
            file.metadata().await?.is_file(),
            "bundle is not a regular file"
        );
        let mut target = tokio::fs::File::create(&snapshot).await?;
        let copied = tokio::time::timeout_at(
            deadline,
            tokio::io::copy(&mut (&mut file).take(128 * 1024 * 1024 + 1), &mut target),
        )
        .await??;
        ensure!(
            copied <= 128 * 1024 * 1024,
            "repository bundle exceeds 128 MiB"
        );
        target.flush().await?;
        drop(target);
        let bytes = tokio::fs::read(&snapshot).await?;
        ensure!(
            agent_core::sha256_digest(&bytes) == spec.repository_digest,
            "immutable repository digest mismatch"
        );
        let workspace = Self {
            root,
            checkpoint_home: PathBuf::new(),
            repository,
            metadata,
            scratch,
            cancel,
            deadline,
        };
        let mut command = git_command();
        command
            .args(["clone", "--quiet", "--no-checkout", "--separate-git-dir"])
            .arg(&workspace.metadata)
            .arg(&snapshot)
            .arg(&workspace.repository);
        workspace.output(command, b"").await?;
        workspace
            .git(
                &["checkout", "--quiet", "--detach", &spec.base_revision, "--"],
                b"",
            )
            .await?;
        Ok(workspace)
    }
    pub(super) async fn prepare(
        &self,
        session: &CodingSession,
        spec: &CodingTurnSpec,
    ) -> Result<String> {
        let prompt = if spec.role == CodingRole::Implement {
            if !session.patch().is_empty() {
                self.apply(session.patch()).await?;
            }
            if let Some(remediation) = &spec.remediation {
                ensure!(
                    patch_digest(session.patch()) == remediation.prior_review.artifact_digest,
                    "remediation refers to another checkpoint artifact"
                );
            }
            format!(
                "Work only in the current repository {}. This workspace was reconstructed from the accepted checkpoint; paths from prior turns are stale.\nRemediation: {}\nTask:\n{}",
                self.repository.display(),
                serde_json::to_string(&spec.remediation)?,
                spec.prompt
            )
        } else {
            let review = spec.review_input.as_ref().context("review input missing")?;
            self.apply(&review.candidate_patch).await?;
            ensure!(
                self.diff().await? == review.candidate_patch,
                "review candidate is not canonical"
            );
            format!(
                "Review the exact candidate at {}. This is a fresh read-only candidate; discard previous path and file-content observations, but retain the review discussion. Scratch is {}. Do not modify the candidate. Return ONLY a JSON CodingReviewResult with schemaVersion:1, reviewId:{}, artifactDigest:{}, verdict (approved or changes-required), findings:[{{findingId,severity,repository,location,summary,evidence,requiredResolution}}], validationGaps:[string]. Approved must have no findings.\nRequirements:\n{}\nImplementation validation evidence:\n{}\nPrior finding ledger:\n{}\nTask:\n{}",
                self.repository.display(),
                self.scratch.display(),
                serde_json::to_string(&review.review_id)?,
                serde_json::to_string(&review.implementation.patch_digest)?,
                review.requirements,
                serde_json::to_string(&review.implementation.validation_evidence)?,
                serde_json::to_string(&review.prior_review)?,
                spec.prompt
            )
        };
        ensure!(
            prompt.len() <= 512 * 1024,
            "expanded Claude prompt too large"
        );
        Ok(prompt)
    }
    pub(super) fn confine(
        &self,
        command: &mut Command,
        host: &HostContext,
        spec: &CodingTurnSpec,
        source: PermissionSource,
    ) -> Result<()> {
        if spec.role == CodingRole::Review {
            command.arg("--json-schema").arg(serde_json::to_string(
                &crate::codex_app_server::coding_review_output_schema(),
            )?);
        }
        let mut writable = Vec::new();
        if spec.role == CodingRole::Implement {
            for root in &spec.writable_roots {
                let suffix = root
                    .strip_prefix(&spec.workspace_root)
                    .filter(|s| s.is_empty() || s.starts_with('/'))
                    .context("write root outside repository")?;
                let path = self
                    .repository
                    .join(suffix.trim_start_matches('/'))
                    .canonicalize()?;
                ensure!(
                    path.starts_with(&self.repository),
                    "write root escapes repository"
                );
                writable.push(path);
            }
        }
        command
            .env("TMPDIR", &self.scratch)
            .env("CARGO_TARGET_DIR", self.scratch.join("target"))
            .env("GRADLE_USER_HOME", self.scratch.join("gradle"));
        native_namespace::wrap(
            command,
            host,
            spec,
            &self.repository,
            &self.metadata,
            &self.scratch,
            &writable,
            source,
        )
    }
    async fn apply(&self, patch: &str) -> Result<()> {
        self.git(
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
        )
        .await?;
        Ok(())
    }
    async fn diff(&self) -> Result<String> {
        // Metadata is outside every CLI-writable root. Never trust an edited .git
        // pointer, HEAD, repository config, replacement refs, or external diff driver.
        // Include new source files, but respect repository ignore rules for
        // dependency trees and build output. Tracked changes remain visible even
        // if a later ignore rule matches them; protected-path checks still run.
        self.git(&["add", "--intent-to-add", "--", "."], b"")
            .await?;
        self.git(
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "HEAD",
                "--",
            ],
            b"",
        )
        .await
    }
    async fn git(&self, args: &[&str], input: &[u8]) -> Result<String> {
        let mut command = git_command();
        command
            .arg("--git-dir")
            .arg(&self.metadata)
            .arg("--work-tree")
            .arg(&self.repository)
            .args(args)
            .current_dir(&self.repository);
        self.output(command, input).await
    }
    async fn output(&self, command: Command, input: &[u8]) -> Result<String> {
        let mut bytes = Vec::new();
        let success = supervise(
            command,
            input,
            self.cancel.clone(),
            self.deadline,
            None,
            |line| {
                ensure!(
                    bytes.len() + line.len() <= 1024 * 1024,
                    "Git output limit exceeded"
                );
                bytes.extend_from_slice(line);
                Ok(Vec::new())
            },
        )
        .await?;
        ensure!(success, "immutable workspace Git operation failed");
        String::from_utf8(bytes).context("Git output is not UTF-8")
    }
}
fn git_command() -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
    command
}

#[cfg(test)]
mod tests;
