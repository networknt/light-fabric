use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;
use workflow_store::{ExpectedBinding, ValidationError};

#[tokio::test]
async fn workflow_binding_and_restart_state_are_durable() {
    let Ok(database_url) = std::env::var("WORKFLOW_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(
        &std::env::var("WORKFLOW_STORE_TEST_BINDING_ID").expect("binding ID accompanies URL"),
    )
    .expect("valid binding ID");
    let host_id = Uuid::parse_str(
        &std::env::var("WORKFLOW_STORE_TEST_HOST_ID").expect("Host ID accompanies URL"),
    )
    .expect("valid Host ID");
    let digest =
        std::env::var("WORKFLOW_STORE_TEST_BINDING_DIGEST").expect("digest accompanies URL");
    let environment =
        std::env::var("WORKFLOW_STORE_TEST_ENVIRONMENT").expect("environment accompanies URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect Workflow store");

    workflow_store::validate(
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
    .expect("exact Workflow binding is ready");
    assert!(matches!(
        workflow_store::validate(
            &pool,
            &ExpectedBinding {
                binding_id,
                binding_digest: &digest,
                host_id: Uuid::now_v7(),
                environment: &environment,
                minimum_schema_generation: 1,
            }
        )
        .await,
        Err(ValidationError::Scope(_))
    ));

    let process_id = Uuid::now_v7();
    let task_id = Uuid::now_v7();
    let approval_id = Uuid::now_v7();
    let definition_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workflow_ops.wf_definition_t(host_id,wf_def_id,namespace,name,version,definition,
           lifecycle_status) VALUES($1,$2,'phase5',$3,'1.0.0','document: {}','PUBLISHED')",
    )
    .bind(host_id)
    .bind(definition_id)
    .bind(format!("restart-{definition_id}"))
    .execute(&pool)
    .await
    .expect("persist accepted definition projection");
    sqlx::query(
        "INSERT INTO workflow_ops.process_info_t(host_id,process_id,wf_def_id,wf_instance_id,
           app_id,process_type,status_code,ex_trigger_ts,context_data)
         VALUES($1,$2,$3,$4,'phase5','workflow','R',now(),'{}'::jsonb)",
    )
    .bind(host_id)
    .bind(process_id)
    .bind(definition_id)
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await
    .expect("persist process");
    sqlx::query(
        "INSERT INTO workflow_ops.task_info_t(host_id,task_id,task_type,process_id,wf_instance_id,
           wf_task_id,status_code,locked,priority,deadline_ts,next_attempt_ts)
         VALUES($1,$2,'wait',$3,$4,'approval','W','N',0,now()+interval '5 minutes',
                now()+interval '1 minute')",
    )
    .bind(host_id)
    .bind(task_id)
    .bind(process_id)
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await
    .expect("persist timer/retry state");
    sqlx::query(
        "INSERT INTO workflow_ops.workflow_approval_t(host_id,approval_id,process_id,task_id,
           target,operation,policy_digest,state,expires_ts)
         VALUES($1,$2,$3,$4,'deployment','approve',$5,'REQUESTED',now()+interval '5 minutes')",
    )
    .bind(host_id)
    .bind(approval_id)
    .bind(process_id)
    .bind(task_id)
    .bind("a".repeat(64))
    .execute(&pool)
    .await
    .expect("persist approval");
    pool.close().await;

    let restarted = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("restart Workflow repository");
    let state: (String, String, String) = sqlx::query_as(
        "SELECT p.status_code::text,t.status_code::text,a.state
           FROM workflow_ops.process_info_t p
           JOIN workflow_ops.task_info_t t ON t.host_id=p.host_id AND t.process_id=p.process_id
           JOIN workflow_ops.workflow_approval_t a ON a.host_id=t.host_id AND a.task_id=t.task_id
          WHERE p.host_id=$1 AND p.process_id=$2",
    )
    .bind(host_id)
    .bind(process_id)
    .fetch_one(&restarted)
    .await
    .expect("reload durable Workflow state");
    assert_eq!(state, ("R".into(), "W".into(), "REQUESTED".into()));
}
