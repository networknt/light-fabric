use execution_backend::{
    BackendError, BackendOperationState, ExecutionBackend, StagedInput, validate_artifact_manifest,
};
use execution_runner_protocol::ExecuteLease;
use tokio::sync::watch;

pub async fn exercise_validation_guards<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
    staged: &[StagedInput],
) -> Result<(), String> {
    backend.validate(lease, staged).map_err(display)?;
    let mut wrong_backend = lease.clone();
    wrong_backend.backend_id = format!("{}-wrong", lease.backend_id);
    if backend.validate(&wrong_backend, staged).is_ok() {
        return Err("backend accepted a lease selecting a different backend".into());
    }
    let mut wrong_compatibility = lease.clone();
    wrong_compatibility.lease.compatibility_digest = "sha256:incompatible".into();
    if backend.validate(&wrong_compatibility, staged).is_ok() {
        return Err("backend accepted an incompatible lease digest".into());
    }
    Ok(())
}

/// Exercises the lifecycle invariants every production backend must preserve.
pub async fn exercise_lifecycle<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
    staged: &[StagedInput],
) -> Result<(), String> {
    backend.validate(lease, staged).map_err(display)?;
    let first = backend.prepare(lease, staged).await.map_err(display)?;
    let duplicate = backend.prepare(lease, staged).await.map_err(display)?;
    if first.backend_operation_id != duplicate.backend_operation_id {
        return Err("duplicate prepare created a different backend operation".into());
    }
    let prepared = backend
        .inspect(&first.backend_operation_id)
        .await
        .map_err(display)?;
    if !matches!(
        prepared.state,
        BackendOperationState::Prepared | BackendOperationState::Running
    ) {
        return Err(format!("prepared operation reported {:?}", prepared.state));
    }
    let (_cancel, cancellation) = watch::channel(false);
    let output = backend
        .execute(&first, lease, cancellation)
        .await
        .map_err(display)?;
    if output.finished_at < output.started_at {
        return Err("backend output timestamps are reversed".into());
    }
    let terminal = backend
        .inspect(&first.backend_operation_id)
        .await
        .map_err(display)?;
    if terminal.state != BackendOperationState::Succeeded {
        return Err(format!(
            "successful execution inspected as {:?}",
            terminal.state
        ));
    }
    let (_duplicate_cancel, duplicate_cancellation) = watch::channel(false);
    match backend.execute(&first, lease, duplicate_cancellation).await {
        Ok(duplicate_output) if duplicate_output == output => {}
        Err(BackendError::Unknown(_)) => {
            let after_duplicate = backend
                .inspect(&first.backend_operation_id)
                .await
                .map_err(display)?;
            if after_duplicate.state != terminal.state {
                return Err("duplicate execute changed an inspected terminal outcome".into());
            }
        }
        Ok(_) => return Err("duplicate execute returned different terminal output".into()),
        Err(error) => return Err(display(error)),
    }
    let artifacts = backend
        .collect_artifacts(&first.backend_operation_id)
        .await
        .map_err(display)?;
    validate_artifact_manifest(&artifacts, 1024, 1024 * 1024 * 1024).map_err(display)?;
    exercise_cleanup(backend, &first.backend_operation_id).await
}

pub async fn exercise_nonzero_failure<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    let (_cancel, cancellation) = watch::channel(false);
    let output = backend
        .execute(&prepared, lease, cancellation)
        .await
        .map_err(display)?;
    if output.exit_code == Some(0) || output.failure_class.is_none() {
        return Err("failed scenario did not return non-zero failure evidence".into());
    }
    let inspection = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    if inspection.state != BackendOperationState::Failed {
        return Err(format!(
            "non-zero execution inspected as {:?}",
            inspection.state
        ));
    }
    exercise_cleanup(backend, &prepared.backend_operation_id).await
}

pub async fn exercise_unknown_outcome<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    let (_cancel, cancellation) = watch::channel(false);
    if !matches!(
        backend.execute(&prepared, lease, cancellation).await,
        Err(BackendError::Unknown(_))
    ) {
        return Err("unknown scenario did not return BackendError::Unknown".into());
    }
    let inspection = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    if inspection.state != BackendOperationState::Unknown {
        return Err(format!(
            "unknown execution inspected as {:?}",
            inspection.state
        ));
    }
    exercise_cleanup(backend, &prepared.backend_operation_id).await
}

pub async fn exercise_cancellation<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    let (cancel, cancellation) = watch::channel(false);
    let execution = backend.execute(&prepared, lease, cancellation);
    tokio::pin!(execution);
    tokio::select! {
        result = &mut execution => return Err(format!("cancellation scenario completed before cancellation: {result:?}")),
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
    }
    cancel
        .send(true)
        .map_err(|_| "backend dropped cancellation receiver".to_string())?;
    if !matches!(execution.await, Err(BackendError::Cancelled(_))) {
        return Err("cancelled scenario did not return BackendError::Cancelled".into());
    }
    let inspection = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    if inspection.state != BackendOperationState::Cancelled {
        return Err(format!(
            "cancelled execution inspected as {:?}",
            inspection.state
        ));
    }
    exercise_cleanup(backend, &prepared.backend_operation_id).await
}

pub async fn exercise_lost_terminal_response<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    let (_cancel, cancellation) = watch::channel(false);
    if !matches!(
        backend.execute(&prepared, lease, cancellation).await,
        Err(BackendError::Transport(_))
    ) {
        return Err("lost-response scenario did not return a transport error".into());
    }
    let inspection = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    if !matches!(
        inspection.state,
        BackendOperationState::Succeeded | BackendOperationState::Failed
    ) {
        return Err(format!(
            "lost terminal response did not leave an inspectable terminal operation: {:?}",
            inspection.state
        ));
    }
    let (_retry_cancel, retry_cancellation) = watch::channel(false);
    backend
        .execute(&prepared, lease, retry_cancellation)
        .await
        .map_err(display)?;
    exercise_cleanup(backend, &prepared.backend_operation_id).await
}

pub async fn exercise_deadline<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    let (_cancel, cancellation) = watch::channel(false);
    if !matches!(
        backend.execute(&prepared, lease, cancellation).await,
        Err(BackendError::TimedOut(_))
    ) {
        return Err("deadline scenario did not return BackendError::TimedOut".into());
    }
    let inspection = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    if inspection.state == BackendOperationState::Running {
        return Err("timed-out operation remained running".into());
    }
    exercise_cleanup(backend, &prepared.backend_operation_id).await
}

pub async fn exercise_cleanup_retry<B: ExecutionBackend>(
    backend: &B,
    lease: &ExecuteLease,
) -> Result<(), String> {
    let prepared = backend.prepare(lease, &[]).await.map_err(display)?;
    if !matches!(
        backend.cleanup(&prepared.backend_operation_id).await,
        Err(BackendError::Cleanup(_))
    ) {
        return Err("cleanup fault scenario did not expose BackendError::Cleanup".into());
    }
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .map_err(display)?;
    match backend.inspect(&prepared.backend_operation_id).await {
        Ok(value) if value.state == BackendOperationState::Cleaned => Ok(()),
        Err(BackendError::NotFound(_)) => Ok(()),
        Ok(value) => Err(format!("cleanup retry left operation in {:?}", value.state)),
        Err(error) => Err(display(error)),
    }
}

async fn exercise_cleanup<B: ExecutionBackend>(
    backend: &B,
    operation_id: &str,
) -> Result<(), String> {
    backend.cleanup(operation_id).await.map_err(display)?;
    backend.cleanup(operation_id).await.map_err(display)?;
    match backend.inspect(operation_id).await {
        Ok(value) if value.state == BackendOperationState::Cleaned => Ok(()),
        Err(BackendError::NotFound(_)) => Ok(()),
        Ok(value) => Err(format!("cleanup left operation in {:?}", value.state)),
        Err(error) => Err(display(error)),
    }
}

fn display(error: BackendError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use execution_backend::validate_artifact_manifest;
    use execution_runner_protocol::ArtifactEvidence;

    fn artifact(name: &str, size: u64) -> ArtifactEvidence {
        ArtifactEvidence {
            logical_name: name.into(),
            file_type: "regular-file".into(),
            media_type: "application/octet-stream".into(),
            size,
            digest: format!("sha256:{}", "a".repeat(64)),
            reference: "object://artifact".into(),
        }
    }

    #[test]
    fn artifact_manifest_rejects_traversal_duplicates_and_excess_bytes() {
        assert!(validate_artifact_manifest(&[artifact("../escape", 1)], 10, 10).is_err());
        assert!(validate_artifact_manifest(&[artifact("/absolute", 1)], 10, 10).is_err());
        assert!(validate_artifact_manifest(&[artifact("a\\b", 1)], 10, 10).is_err());
        assert!(
            validate_artifact_manifest(&[artifact("same", 1), artifact("same", 1)], 10, 10)
                .is_err()
        );
        assert!(validate_artifact_manifest(&[artifact("large", 11)], 10, 10).is_err());
    }

    #[test]
    fn artifact_manifest_rejects_bad_digest_and_entry_count() {
        let mut invalid = artifact("safe", 1);
        invalid.digest = "sha256:short".into();
        assert!(validate_artifact_manifest(&[invalid], 10, 10).is_err());
        assert!(
            validate_artifact_manifest(&[artifact("one", 1), artifact("two", 1)], 1, 10).is_err()
        );
        let mut symlink = artifact("link", 1);
        symlink.file_type = "symlink".into();
        assert!(validate_artifact_manifest(&[symlink], 10, 10).is_err());
    }
}
