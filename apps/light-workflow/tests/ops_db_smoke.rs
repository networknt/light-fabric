#[path = "support/ops_db.rs"]
mod ops_db;

use sqlx::Error;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn ledger_lists_last_workflow_migration_in_bundle() {
    let pool = ops_db::runtime_pool();
    let migration_id = ops_db::last_workflow_migration_id();
    let applied: bool = sqlx::query_scalar(
        "SELECT EXISTS (\
           SELECT 1 FROM operational_meta.operational_schema_migration_t \
           WHERE migration_owner = 'workflow-store' \
             AND schema_name = 'workflow_ops' \
             AND migration_id = $1\
         )",
    )
    .bind(&migration_id)
    .fetch_one(&pool)
    .await
    .expect("read the operational migration ledger");

    assert!(
        applied,
        "last Workflow migration {migration_id} is absent from the ledger"
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn runtime_role_can_insert_and_read_a_definition() {
    let pool = ops_db::runtime_pool();
    let host_id = ops_db::random_host_id();
    let (wf_def_id, expected_name) = ops_db::insert_workflow_definition(&pool, host_id)
        .await
        .expect("runtime role inserts a workflow definition");
    let actual_name = ops_db::definition_name(&pool, wf_def_id)
        .await
        .expect("runtime role reads the workflow definition");

    assert_eq!(actual_name, expected_name);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn runtime_role_cannot_create_tables_in_workflow_schema() {
    let pool = ops_db::runtime_pool();
    let table_name = format!("ops_smoke_forbidden_{}", Uuid::new_v4().simple());
    let error = sqlx::query(&format!(
        "CREATE TABLE workflow_ops.{table_name} (id integer)"
    ))
    .execute(&pool)
    .await
    .expect_err("runtime role must not create tables in workflow_ops");

    assert!(
        matches!(error, Error::Database(ref database_error) if database_error.code().as_deref() == Some("42501")),
        "expected PostgreSQL insufficient_privilege (42501), got {error}"
    );
}
