use a2a_core::{A2aError, AuthorizedInvocation, Direction, TaskState};
use agent_store::{
    NativeA2aError, NativeA2aRepository, NativeTaskAccess, NativeTaskAdmission,
    NativeTaskListAccess,
};
use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn native_a2a_aliases_use_agent_state_and_survive_restart() {
    let Ok(database_url) = std::env::var("AGENT_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let host_id = Uuid::parse_str(&std::env::var("AGENT_STORE_TEST_HOST_ID").unwrap()).unwrap();
    let agent_def_id = Uuid::now_v7();
    let publication_id = Uuid::now_v7();
    let policy_snapshot_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let turn_id = Uuid::now_v7();
    let task_id = Uuid::now_v7();
    let context_id = Uuid::now_v7();
    let skill_id = Uuid::now_v7();
    let policy_digest = format!("sha256:{}", "a".repeat(64));
    let digest = format!("sha256:{}", "b".repeat(64));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO agent_policy_snapshot_t(host_id,policy_snapshot_id,agent_def_id,
      agent_definition_version,agent_publication_id,agent_content_digest,definition_digest,
      product_profile_digest,model_digest,catalog_digest,memory_digest,execution_digest,
      channel_digest,data_boundary_digest,resolved_snapshot,policy_digest)
      VALUES($1,$2,$3,1,$4,$5,$5,$5,$5,$5,$5,$5,$5,$5,'{}'::jsonb,$6)",
    )
    .bind(host_id)
    .bind(policy_snapshot_id)
    .bind(agent_def_id)
    .bind(publication_id)
    .bind(&digest)
    .bind(&policy_digest)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO agent_session_t(host_id,session_id,principal_id,agent_def_id,
      agent_definition_version,policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest,
      agent_publication_id,agent_content_digest,agent_definition_digest,user_identity_digest,model_provider,model_name)
      VALUES($1,$2,'workflow:phase5',$3,1,$4,now()+interval '1 hour',now()+interval '2 hours',$5,$6,$5,$5,$5,'mock','mock')")
        .bind(host_id).bind(session_id).bind(agent_def_id).bind(policy_snapshot_id)
        .bind(&digest).bind(publication_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO agent_turn_t(host_id,turn_id,session_id,turn_sequence,queue_sequence,
      origin_kind,origin_ref,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,
      data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,cost_budget_micros,deadline_ts)
      VALUES($1,$2,$3,1,1,'workflow','phase5','phase5-message','phase5-idempotency',$4,$5,$6,
      'mock','mock',1,1024,0,now()+interval '10 minutes')")
        .bind(host_id).bind(turn_id).bind(session_id).bind(policy_snapshot_id)
        .bind(&policy_digest).bind(&digest).execute(&pool).await.unwrap();

    let now = Utc::now();
    let invocation = AuthorizedInvocation {
        host_id,
        audience: "light-agent".into(),
        principal_subject: "workflow:phase5".into(),
        caller_agent_ref: "workflow:qualification".into(),
        target_agent_ref: "account.agent".into(),
        binding_id: Uuid::now_v7(),
        policy_digest: policy_digest.clone(),
        publication_id,
        direction: Direction::Inbound,
        idempotency_key: "phase5-idempotency".into(),
        request_digest: digest.clone(),
        outbound: None,
        issued_at: now,
        expires_at: now + chrono::Duration::minutes(10),
    };
    let repository = NativeA2aRepository::new(pool.clone());
    let admitted = repository
        .bind(&NativeTaskAdmission {
            session_id,
            turn_id,
            task_id,
            context_id,
            agent_def_id,
            message_id: "phase4-message".into(),
            skill_mapping: serde_json::json!([{
                "publicationAlias":"account.lookup",
                "skillId":skill_id,
                "skillVersion":"1.0.0",
                "skillDigest":digest.clone()
            }]),
            skill_mapping_digest: format!("sha256:{}", "c".repeat(64)),
            invocation: invocation.clone(),
        })
        .await
        .unwrap();
    assert_eq!(admitted.state, TaskState::Submitted);
    let duplicate = repository
        .bind(&NativeTaskAdmission {
            session_id,
            turn_id,
            task_id,
            context_id,
            agent_def_id,
            message_id: "phase4-message".into(),
            skill_mapping: serde_json::json!([{
                "publicationAlias":"account.lookup",
                "skillId":skill_id,
                "skillVersion":"1.0.0",
                "skillDigest":digest.clone()
            }]),
            skill_mapping_digest: format!("sha256:{}", "c".repeat(64)),
            invocation: invocation.clone(),
        })
        .await
        .unwrap();
    assert_eq!(duplicate.task_id, task_id);
    let persisted_mapping_digest: String = sqlx::query_scalar(
        "SELECT skill_mapping_digest FROM agent_a2a_task_alias_t
          WHERE host_id=$1 AND public_task_id=$2",
    )
    .bind(host_id)
    .bind(task_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        persisted_mapping_digest,
        format!("sha256:{}", "c".repeat(64))
    );

    let access = NativeTaskAccess {
        host_id,
        task_id,
        principal_subject: "workflow:phase5",
        target_agent_id: agent_def_id,
        publication_id,
    };
    assert_eq!(
        repository.resolve_turn(&access).await.unwrap(),
        (session_id, turn_id)
    );
    let wrong = NativeTaskAccess {
        host_id,
        task_id,
        principal_subject: "wrong",
        target_agent_id: agent_def_id,
        publication_id,
    };
    assert!(matches!(
        repository.get(&wrong).await,
        Err(NativeA2aError::A2a(A2aError::WrongTaskOwner))
    ));
    let wrong_agent = NativeTaskAccess {
        host_id,
        task_id,
        principal_subject: "workflow:phase5",
        target_agent_id: Uuid::now_v7(),
        publication_id,
    };
    assert!(matches!(
        repository.get(&wrong_agent).await,
        Err(NativeA2aError::A2a(A2aError::WrongTaskOwner))
    ));
    let submitted = repository
        .list(&NativeTaskListAccess {
            host_id,
            principal_subject: "workflow:phase5",
            target_agent_id: agent_def_id,
            publication_id,
            context_id: Some(context_id),
            status: Some(TaskState::Submitted),
            status_timestamp_after: None,
            cursor: None,
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(submitted.total_size, 1);
    assert_eq!(submitted.tasks.len(), 1);
    assert!(submitted.next_cursor.is_none());

    pool.close().await;
    let restarted = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let repository = NativeA2aRepository::new(restarted.clone());
    assert_eq!(
        repository.get(&access).await.unwrap().context_id,
        context_id
    );
    assert_eq!(
        repository.mark_canceled(&access).await.unwrap().state,
        TaskState::Canceled
    );
    let canceled = repository
        .list(&NativeTaskListAccess {
            host_id,
            principal_subject: "workflow:phase5",
            target_agent_id: agent_def_id,
            publication_id,
            context_id: None,
            status: Some(TaskState::Canceled),
            status_timestamp_after: Some(Utc::now() - chrono::Duration::minutes(1)),
            cursor: None,
            limit: 50,
        })
        .await
        .unwrap();
    assert_eq!(canceled.total_size, 1);
    assert_eq!(
        repository.mark_canceled(&access).await.unwrap().state,
        TaskState::Canceled
    );

    let artifact_id = Uuid::now_v7();
    let artifact_digest = format!("sha256:{}", "d".repeat(64));
    repository
        .register_artifact(
            &access,
            &agent_store::NativeArtifactAdmission {
                artifact_id,
                logical_name: "response.json",
                media_type: "application/json",
                size_bytes: 2,
                content_digest: &artifact_digest,
                object_reference: &format!("agent-turn-result:{}", turn_id),
                provenance_digest: &digest,
                retain_until: Utc::now() + chrono::Duration::seconds(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(repository.get(&access).await.unwrap().artifacts.len(), 1);
    repository
        .expire_artifacts(host_id, Utc::now() + chrono::Duration::seconds(2))
        .await
        .unwrap();
    assert!(repository.get(&access).await.unwrap().artifacts.is_empty());
    let turn_survives_expiry: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2)",
    )
    .bind(host_id)
    .bind(turn_id)
    .fetch_one(&restarted)
    .await
    .unwrap();
    assert!(turn_survives_expiry);
}
