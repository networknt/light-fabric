use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;
use workflow_action::ledger::{Error, Ledger, MIGRATION};
use workflow_action::*;
fn binding() -> Binding {
    let hash = request_digest("POST", "https://approved/mcp", "tool", b"{}");
    Binding {
        host_id: Uuid::now_v7(),
        user_id: Uuid::now_v7(),
        grant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        action_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        calling_app: "workflow".into(),
        request_digest: hash.clone(),
        request_bytes: 2,
        response_byte_limit: 1024,
        cost_unit_limit: 1,
        tool_ref: Uuid::now_v7(),
        target: "https://approved/mcp".into(),
        contract_digest: hash.clone(),
        policy_digest: hash.clone(),
        disclosure_digest: hash.clone(),
        claims_digest: hash,
        grant_generation: 1,
        run_generation: 1,
        budget_generation: 1,
        action_generation: 1,
        execution_class: ExecutionClass::Standard,
        depth: 0,
        maximum_depth: 4,
        parent_action_id: None,
        deadline: chrono::DateTime::from_timestamp_micros(
            (chrono::Utc::now() + chrono::Duration::minutes(2)).timestamp_micros(),
        )
        .unwrap(),
    }
}
fn owner() -> Owner {
    Owner {
        gateway_service: "gateway".into(),
        replica: Uuid::now_v7(),
        boot: Uuid::now_v7(),
        fencing_generation: 1,
    }
}
#[tokio::test]
#[ignore = "requires a dedicated disposable WORKFLOW_ACTION_TEST_DATABASE_URL"]
async fn durable_claims_replay_fencing_and_not_initiated() {
    let url = std::env::var("WORKFLOW_ACTION_TEST_DATABASE_URL")
        .expect("explicit disposable database required");
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE SCHEMA workflow_ops")
        .execute(&pool)
        .await
        .unwrap();
    let runtime =
        include_str!("../../workflow-store/migrations/workflow-postgres/0001_workflow_runtime.sql");
    // Use the real schema/constraints; role grants are a separate installer gate.
    sqlx::raw_sql(runtime.split("GRANT USAGE ON SCHEMA").next().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    let b = binding();
    let l = Ledger::new(pool.clone());
    let original = owner();
    let peer = "a".repeat(64);
    let registration = RegisterOwner {
        gateway_service: original.gateway_service.clone(),
        replica: original.replica,
        boot: original.boot,
    };
    let o = l.register_owner(&peer, &registration).await.unwrap();
    assert_eq!(o, l.register_owner(&peer, &registration).await.unwrap());
    let def = Uuid::now_v7();
    let tool_binding = Uuid::now_v7();
    sqlx::query("INSERT INTO workflow_ops.wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'test','action','1','test')").bind(b.host_id).bind(def).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_ops.workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest) VALUES($1,$2,$3,$4,'1',$5,$5,'async',1,120000,'standard','compact-json','{}','{}',$5,'{}',$5)")
        .bind(b.host_id).bind(tool_binding).bind(b.tool_ref).bind(def).bind(&b.policy_digest).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_ops.workflow_invocation_t(host_id,workflow_instance_id,binding_id,stable_tool_ref,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,principal_subject,end_user_subject,input,input_digest,canonical_input_profile,invocation_mode,execution_class,state,correlation_id,deadline_ts) VALUES($1,$2,$3,$4,$5,'1',$6,$6,$6,$6,'gateway',$7,'{}',$6,'rfc8785-safe-json-v1','async','standard','RUNNING','test',$8)")
        .bind(b.host_id).bind(b.run_id).bind(tool_binding).bind(b.tool_ref).bind(def).bind(&b.policy_digest).bind(b.user_id.to_string()).bind(b.deadline).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_ops.workflow_invocation_budget_t(host_id,ledger_id,workflow_instance_id,task_attempt_limit,nested_call_limit,byte_limit,cost_unit_limit,deadline_ts) VALUES($1,$2,$3,100,10,100000,100,$4)")
        .bind(b.host_id).bind(Uuid::now_v7()).bind(b.run_id).bind(b.deadline).execute(&pool).await.unwrap();
    l.admit_run(b.host_id, b.run_id, b.grant_id, b.user_id)
        .await
        .unwrap();
    l.install_permit(&b, 10).await.unwrap();
    let reference = ActionReference {
        host_id: b.host_id,
        action_id: b.action_id,
        calling_app: b.calling_app.clone(),
        request_digest: b.request_digest.clone(),
        tool_ref: b.tool_ref,
        target: b.target.clone(),
        contract_digest: b.contract_digest.clone(),
    };
    assert_eq!(l.resolve(&reference).await.unwrap(), b);
    let mut forged = reference.clone();
    forged.calling_app = "interactive-agent".into();
    assert!(matches!(l.resolve(&forged).await, Err(Error::Denied)));
    forged = reference.clone();
    forged.request_digest = request_digest("POST", "changed", "tool", b"{}");
    assert!(matches!(l.resolve(&forged).await, Err(Error::Denied)));
    forged = reference.clone();
    forged.tool_ref = Uuid::now_v7();
    assert!(matches!(l.resolve(&forged).await, Err(Error::Denied)));
    let (a, c) = tokio::join!(l.authorize(&b, &o), l.authorize(&b, &o));
    let d = a.unwrap();
    assert_eq!(d, c.unwrap());
    assert!(matches!(
        l.authorize(&b, &owner()).await,
        Err(Error::Denied)
    ));
    let mut altered = b.clone();
    altered.request_digest = request_digest("POST", "other", "tool", b"{}");
    assert!(matches!(
        l.authorize(&altered, &o).await,
        Err(Error::Conflict)
    ));
    let (a, c) = tokio::join!(l.begin(&d), l.begin(&d));
    assert_ne!(a.unwrap(), c.unwrap());
    let completion = Completion {
        decision: d.clone(),
        outcome: DispatchState::NotInitiated,
        evidence_digest: None,
    };
    l.complete(&completion).await.unwrap();
    l.complete(&completion).await.unwrap();
    let d2 = l.authorize(&b, &o).await.unwrap();
    assert_eq!(d2.generation, 2);
    // Lost old completion reply must not release the new reservation.
    l.complete(&completion).await.unwrap();
    let reserved: i64 =
        sqlx::query_scalar("SELECT reserved FROM workflow_ops.workflow_action_authority_t")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reserved, 1);
    assert!(matches!(l.begin(&d).await, Err(Error::Conflict)));
    assert!(l.begin(&d2).await.unwrap());
    // Cancellation blocks new authority but still permits bound evidence reporting.
    sqlx::query(
        "UPDATE workflow_ops.workflow_invocation_t SET cancel_requested_ts=clock_timestamp()",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(l.status(&d2).await, Err(Error::Denied)));
    l.complete(&Completion {
        decision: d2.clone(),
        outcome: DispatchState::Uncertain,
        evidence_digest: None,
    })
    .await
    .unwrap();
    sqlx::query("UPDATE workflow_ops.workflow_invocation_t SET cancel_requested_ts=NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(l.authorize(&b, &o).await, Err(Error::Uncertain)));
    assert!(
        l.complete(&Completion {
            decision: d2.clone(),
            outcome: DispatchState::NotInitiated,
            evidence_digest: None
        })
        .await
        .is_err()
    );

    let next = RegisterOwner {
        boot: Uuid::now_v7(),
        ..registration.clone()
    };
    let new_owner = l.register_owner(&peer, &next).await.unwrap();
    assert_eq!(new_owner.fencing_generation, o.fencing_generation + 1);
    assert!(!l.is_current_owner(&peer, &o).await.unwrap());
    assert!(matches!(
        l.register_owner(&peer, &registration).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(l.begin(&d2).await, Err(Error::Denied)));
    assert_eq!(
        l.latest_status(&b, &new_owner).await.unwrap(),
        DispatchState::Uncertain
    );
    let reconciliation = Reconciliation {
        host_id: b.host_id,
        action_id: b.action_id,
        generation: d2.generation,
        outcome: DispatchState::Succeeded,
        evidence_digest: request_digest("RECEIPT", "qualified-target", "result", b"done"),
    };
    assert_eq!(
        l.reconciliation_binding(b.host_id, b.action_id, d2.generation)
            .await
            .unwrap(),
        b
    );
    l.reconcile(&reconciliation).await.unwrap();
    l.reconcile(&reconciliation).await.unwrap();
    assert_eq!(
        l.latest_status(&b, &new_owner).await.unwrap(),
        DispatchState::Succeeded
    );
    let mut conflicting_receipt = reconciliation.clone();
    conflicting_receipt.evidence_digest =
        request_digest("RECEIPT", "qualified-target", "result", b"different");
    assert!(matches!(
        l.reconcile(&conflicting_receipt).await,
        Err(Error::Conflict)
    ));

    let mut parent = b.clone();
    parent.action_id = Uuid::now_v7();
    parent.attempt_id = Uuid::now_v7();
    parent.request_digest = request_digest(
        "POST",
        &parent.target,
        &parent.tool_ref.to_string(),
        b"child",
    );
    l.install_permit(&parent, 2).await.unwrap();
    let parent_decision = l.authorize(&parent, &new_owner).await.unwrap();
    assert!(l.begin(&parent_decision).await.unwrap());
    assert_eq!(
        l.receiver_parent(
            parent.host_id,
            parent.action_id,
            &peer,
            &new_owner.gateway_service,
        )
        .await
        .unwrap(),
        parent
    );
    assert!(
        l.receiver_parent(
            parent.host_id,
            parent.action_id,
            &"b".repeat(64),
            &new_owner.gateway_service,
        )
        .await
        .is_err()
    );

    let child_run = Uuid::now_v7();
    sqlx::query("INSERT INTO workflow_ops.workflow_invocation_t(host_id,workflow_instance_id,binding_id,stable_tool_ref,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,principal_subject,end_user_subject,input,input_digest,canonical_input_profile,invocation_mode,execution_class,state,correlation_id,permit_depth,deadline_ts) VALUES($1,$2,$3,$4,$5,'1',$6,$6,$6,$6,'gateway',$7,'{}',$6,'rfc8785-safe-json-v1','async','standard','RUNNING','child',1,$8)")
        .bind(b.host_id).bind(child_run).bind(tool_binding).bind(b.tool_ref).bind(def)
        .bind(&b.policy_digest).bind(b.user_id.to_string()).bind(b.deadline).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_ops.workflow_invocation_budget_t(host_id,ledger_id,workflow_instance_id,task_attempt_limit,nested_call_limit,byte_limit,cost_unit_limit,deadline_ts) VALUES($1,$2,$3,100,5,100000,100,$4)")
        .bind(b.host_id).bind(Uuid::now_v7()).bind(child_run).bind(b.deadline).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let inherited =
        Ledger::admit_child_run_in(&mut tx, b.host_id, child_run, b.user_id, parent.action_id)
            .await
            .unwrap();
    assert_eq!(inherited.action_id, parent.action_id);
    tx.commit().await.unwrap();
    let lineage: (Option<Uuid>, Option<Uuid>, i32, i32) = sqlx::query_as(
        "SELECT parent_action_id,parent_run_id,depth,maximum_depth FROM workflow_ops.workflow_action_authority_t WHERE host_id=$1 AND run_id=$2",
    )
    .bind(b.host_id)
    .bind(child_run)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lineage, (Some(parent.action_id), Some(parent.run_id), 1, 4));
    pool.close().await;
}
