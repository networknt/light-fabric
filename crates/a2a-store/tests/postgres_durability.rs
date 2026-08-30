use a2a_core::{A2aError, AuthorizedInvocation, Direction, TaskState};
use a2a_store::{ArtifactMetadata, ExpectedBinding, Repository, TaskAccess, TaskAdmission};
use chrono::{Duration, Utc};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn invocation(host_id: Uuid, binding_id: Uuid, direction: Direction) -> AuthorizedInvocation {
    let now = Utc::now();
    AuthorizedInvocation {
        host_id,
        audience: "light-a2a".into(),
        principal_subject: "user:phase5".into(),
        caller_agent_ref: "workflow:phase5".into(),
        target_agent_ref: "external:account".into(),
        binding_id,
        policy_digest: format!("sha256:{}", "a".repeat(64)),
        publication_id: Uuid::now_v7(),
        direction,
        idempotency_key: format!("phase5-{direction:?}"),
        request_digest: format!("sha256:{}", "b".repeat(64)),
        issued_at: now,
        expires_at: now + Duration::minutes(5),
    }
}

#[tokio::test]
async fn inbound_outbound_ownership_replay_cancel_artifact_and_restart() {
    let Ok(database_url) = std::env::var("A2A_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(
        &std::env::var("A2A_STORE_TEST_BINDING_ID").expect("binding ID accompanies URL"),
    )
    .expect("valid binding ID");
    let host_id =
        Uuid::parse_str(&std::env::var("A2A_STORE_TEST_HOST_ID").expect("Host ID accompanies URL"))
            .expect("valid Host ID");
    let digest = std::env::var("A2A_STORE_TEST_BINDING_DIGEST").expect("digest accompanies URL");
    let environment =
        std::env::var("A2A_STORE_TEST_ENVIRONMENT").expect("environment accompanies URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect A2A store");
    a2a_store::validate(
        &pool,
        &ExpectedBinding {
            binding_id,
            binding_digest: &digest,
            host_id,
            environment: &environment,
            minimum_schema_generation: 1,
        },
    )
    .await
    .expect("exact A2A binding is ready");

    let repository = Repository::new(pool.clone());
    for direction in [Direction::Inbound, Direction::Outbound] {
        let task_id = Uuid::now_v7();
        let invocation = invocation(host_id, binding_id, direction);
        let admission = TaskAdmission {
            task_id,
            context_id: Uuid::now_v7(),
            invocation: invocation.clone(),
        };
        let first = repository.admit(&admission).await.expect("admit A2A task");
        let duplicate = repository
            .admit(&admission)
            .await
            .expect("same request is idempotent");
        assert_eq!(first.task_id, duplicate.task_id);

        let mut replay = admission.clone();
        replay.invocation.request_digest = format!("sha256:{}", "c".repeat(64));
        assert!(matches!(
            repository.admit(&replay).await,
            Err(a2a_store::StoreError::A2a(A2aError::Replay))
        ));
        let access = TaskAccess {
            host_id,
            task_id,
            principal_subject: &invocation.principal_subject,
            caller_agent_ref: &invocation.caller_agent_ref,
            target_agent_ref: &invocation.target_agent_ref,
            binding_id,
        };
        let wrong = TaskAccess {
            principal_subject: "user:other",
            ..access.clone()
        };
        assert!(matches!(
            repository.get(&wrong).await,
            Err(a2a_store::StoreError::A2a(A2aError::WrongTaskOwner))
        ));
        repository
            .bind_backend(
                &access,
                "EXTERNAL_SIDECAR",
                Uuid::now_v7(),
                &format!("backend:{task_id}"),
            )
            .await
            .expect("persist opaque backend correlation");
        repository
            .schedule_callback(
                &access,
                Uuid::now_v7(),
                "TASK_STATUS",
                &format!("callback:{task_id}"),
                Some("vault:a2a-callback"),
            )
            .await
            .expect("persist server-owned callback reference");
        repository
            .reconcile(&access, TaskState::Working, None, None)
            .await
            .expect("reconcile non-terminal backend status");
        repository
            .add_artifact(
                &access,
                &ArtifactMetadata {
                    artifact_id: Uuid::now_v7(),
                    logical_name: "result.json",
                    media_type: "application/json",
                    size_bytes: 2,
                    content_digest: &format!("sha256:{}", "d".repeat(64)),
                    object_reference: "objects/phase5/result.json",
                    visibility: "AUTHORIZED_CALLER",
                    retain_until: Utc::now() + Duration::days(7),
                },
            )
            .await
            .expect("store artifact metadata only");
        let canceled = repository.cancel(&access).await.expect("cancel owned task");
        assert_eq!(canceled.state, TaskState::Canceled);
    }
    pool.close().await;

    let restarted = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("restart A2A repository");
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM a2a_ops.a2a_task_t),
                (SELECT count(*) FROM a2a_ops.a2a_backend_correlation_t),
                (SELECT count(*) FROM a2a_ops.a2a_callback_t),
                (SELECT count(*) FROM a2a_ops.a2a_artifact_t),
                (SELECT count(*) FROM a2a_ops.a2a_audit_outbox_t)",
    )
    .fetch_one(&restarted)
    .await
    .expect("reload A2A state");
    assert_eq!(counts.0, 2);
    assert_eq!(counts.1, 2);
    assert_eq!(counts.2, 2);
    assert_eq!(counts.3, 2);
    assert!(counts.4 >= 4);
}
