use a2a_core::{A2aError, AuthorizedInvocation, Direction, TaskState};
use agent_store::{NativeA2aError, NativeA2aRepository, NativeTaskAccess, NativeTaskAdmission};
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
            invocation: invocation.clone(),
        })
        .await
        .unwrap();
    assert_eq!(admitted.state, TaskState::Submitted);

    let access = NativeTaskAccess {
        host_id,
        task_id,
        principal_subject: "workflow:phase5",
        target_agent_id: agent_def_id,
        publication_id,
        policy_digest: &policy_digest,
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
        policy_digest: &policy_digest,
    };
    assert!(matches!(
        repository.get(&wrong).await,
        Err(NativeA2aError::A2a(A2aError::WrongTaskOwner))
    ));

    pool.close().await;
    let restarted = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let repository = NativeA2aRepository::new(restarted);
    assert_eq!(
        repository.get(&access).await.unwrap().context_id,
        context_id
    );
    assert_eq!(
        repository.mark_canceled(&access).await.unwrap().state,
        TaskState::Canceled
    );
}
