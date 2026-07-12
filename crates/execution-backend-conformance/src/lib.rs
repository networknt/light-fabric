use execution_backend::{BackendError, BackendOperationState, ExecutionBackend, StagedInput};
use execution_runner_protocol::ExecuteLease;
use tokio::sync::watch;

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
    backend
        .collect_artifacts(&first.backend_operation_id)
        .await
        .map_err(display)?;
    backend
        .cleanup(&first.backend_operation_id)
        .await
        .map_err(display)?;
    backend
        .cleanup(&first.backend_operation_id)
        .await
        .map_err(display)?;
    match backend.inspect(&first.backend_operation_id).await {
        Ok(value) if value.state == BackendOperationState::Cleaned => Ok(()),
        Err(BackendError::NotFound(_)) => Ok(()),
        Ok(value) => Err(format!("cleanup left operation in {:?}", value.state)),
        Err(error) => Err(display(error)),
    }
}

fn display(error: BackendError) -> String {
    error.to_string()
}
