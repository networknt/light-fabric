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
        outbound: (direction == Direction::Outbound).then(|| {
            a2a_core::OutboundInvocationConstraints {
                delegation_id: Uuid::now_v7(),
                environment: "dev".into(),
                data_boundary_digest: format!("sha256:{}", "c".repeat(64)),
                delegation_depth: 1,
                maximum_delegation_depth: 4,
                remaining_budget_units: 100,
                deadline: now + Duration::minutes(1),
                call_chain: vec!["caller.agent".into()],
                skill_id: None,
            }
        }),
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
        if direction == Direction::Outbound {
            let remote_binding_id = Uuid::now_v7();
            repository
                .bind_remote_task(
                    &access,
                    remote_binding_id,
                    &format!("remote:{task_id}"),
                    Some("remote-context"),
                    Some("account.lookup"),
                )
                .await
                .expect("persist owned remote A2A identity");
            let remote = repository
                .backend_task_binding(&access)
                .await
                .expect("reload remote task binding");
            assert_eq!(remote.backend_kind, "REMOTE_A2A");
            assert_eq!(remote.backend_binding_id, remote_binding_id);
            assert_eq!(
                remote.remote_task_id.as_deref(),
                Some(&*format!("remote:{task_id}"))
            );
            assert_eq!(remote.remote_context_id.as_deref(), Some("remote-context"));
            assert!(matches!(
                repository.backend_task_binding(&wrong).await,
                Err(a2a_store::StoreError::A2a(A2aError::WrongTaskOwner))
            ));
        } else {
            repository
                .bind_backend(
                    &access,
                    "EXTERNAL_SIDECAR",
                    Uuid::now_v7(),
                    &format!("backend:{task_id}"),
                    Some("account.lookup"),
                )
                .await
                .expect("persist opaque backend correlation");
        }
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
        let push_config = repository
            .create_push_config(
                &access,
                Uuid::now_v7(),
                Uuid::now_v7(),
                &format!("sha256:{}", "e".repeat(64)),
            )
            .await
            .expect("create owned push configuration");
        assert!(matches!(
            repository
                .get_push_config(&wrong, push_config.config_id)
                .await,
            Err(a2a_store::StoreError::A2a(A2aError::WrongTaskOwner))
        ));
        assert_eq!(
            repository
                .enqueue_push_deliveries(
                    &access,
                    &serde_json::json!({"statusUpdate":{"taskId":task_id}}),
                    2,
                )
                .await
                .expect("enqueue durable push delivery"),
            1
        );
        let worker = format!("phase6-worker-{direction:?}");
        let first_delivery = repository
            .claim_push_deliveries(host_id, &worker, 1, 30)
            .await
            .expect("claim push delivery")
            .pop()
            .expect("one delivery");
        if direction == Direction::Inbound {
            repository
                .retry_push_delivery(
                    host_id,
                    first_delivery.delivery_id,
                    &worker,
                    "TEST_RETRY",
                    1,
                    Some(503),
                )
                .await
                .expect("persist bounded retry");
            sqlx::query(
                "UPDATE a2a_ops.a2a_push_delivery_t SET next_attempt_ts=now()
                  WHERE host_id=$1 AND delivery_id=$2",
            )
            .bind(host_id)
            .bind(first_delivery.delivery_id)
            .execute(&pool)
            .await
            .expect("advance disposable retry clock");
            let second_delivery = repository
                .claim_push_deliveries(host_id, &worker, 1, 30)
                .await
                .expect("reclaim push delivery")
                .pop()
                .expect("one retried delivery");
            assert_eq!(second_delivery.attempt, 2);
            repository
                .complete_push_delivery(host_id, second_delivery.delivery_id, &worker, 204)
                .await
                .expect("complete push delivery");
        } else {
            sqlx::query(
                "UPDATE a2a_ops.a2a_push_delivery_t SET lease_until_ts=now()-interval '1 second'
                  WHERE host_id=$1 AND delivery_id=$2",
            )
            .bind(host_id)
            .bind(first_delivery.delivery_id)
            .execute(&pool)
            .await
            .expect("expire disposable worker lease");
            let takeover_worker = "phase7-worker-takeover";
            let takeover = repository
                .claim_push_deliveries(host_id, takeover_worker, 1, 30)
                .await
                .expect("second worker claims expired lease")
                .pop()
                .expect("one expired delivery");
            assert_eq!(takeover.attempt, 2);
            assert!(
                repository
                    .complete_push_delivery(host_id, takeover.delivery_id, &worker, 204)
                    .await
                    .is_err()
            );
            repository
                .retry_push_delivery(
                    host_id,
                    takeover.delivery_id,
                    takeover_worker,
                    "QUALIFICATION_FAILURE",
                    1,
                    Some(503),
                )
                .await
                .expect("exhaust retry budget into dead letter");
        }
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
    let push_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM a2a_ops.a2a_push_config_t),
                (SELECT count(*) FROM a2a_ops.a2a_push_delivery_t),
                (SELECT count(*) FROM a2a_ops.a2a_push_delivery_t WHERE state='DELIVERED'),
                (SELECT count(*) FROM a2a_ops.a2a_push_delivery_t WHERE state='DEAD_LETTER')",
    )
    .fetch_one(&restarted)
    .await
    .expect("reload durable push state");
    assert_eq!(push_counts, (2, 2, 1, 1));
}
