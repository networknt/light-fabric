//! Owner-run only, against an explicitly owned disposable migrated database.
use light_workflow::{
    profile_support::SupportedProfiles,
    worker_capability::{Capability, CapabilityStore},
};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires explicitly owned migrated e04_w6_* W6_TEST_DATABASE_URL; writes/removes only one capability fixture"]
async fn w6_postgres_capability_identity_refresh_and_policy_unchanged() {
    let url = std::env::var("W6_TEST_DATABASE_URL")
        .expect("explicit disposable W6_TEST_DATABASE_URL required");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(database.starts_with("e04_w6_"));
    let policy:serde_json::Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY profile_id) FROM workflow_ops.workflow_expression_profile_policy_t p").fetch_one(&pool).await.unwrap();
    let capability = Capability::new(SupportedProfiles::from_evaluator(true), true);
    // Exercise the accepted runtime privileges, including the real UPSERT.
    sqlx::query("SET ROLE operations_workflow_runtime")
        .execute(&pool)
        .await
        .unwrap();
    pool.write(&capability).await.unwrap();
    sqlx::query("UPDATE workflow_ops.workflow_worker_capability_t SET heartbeat_ts=clock_timestamp()-interval '1 hour' WHERE instance_id=$1").bind(capability.instance_id).execute(&pool).await.unwrap();
    pool.write(&capability).await.unwrap();
    let (version,supported,admitting,fresh):(String,Vec<String>,Vec<String>,bool)=sqlx::query_as("SELECT binary_version,supported_profiles,admits_profiles,heartbeat_ts>clock_timestamp()-interval '1 minute' FROM workflow_ops.workflow_worker_capability_t WHERE instance_id=$1").bind(capability.instance_id).fetch_one(&pool).await.unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    assert_eq!(supported, capability.supported.profiles());
    assert_eq!(admitting, capability.admitting.profiles());
    assert!(fresh);
    sqlx::query("RESET ROLE").execute(&pool).await.unwrap();
    let after:serde_json::Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY profile_id) FROM workflow_ops.workflow_expression_profile_policy_t p").fetch_one(&pool).await.unwrap();
    assert_eq!(policy, after);
    sqlx::query("DELETE FROM workflow_ops.workflow_worker_capability_t WHERE instance_id=$1")
        .bind(capability.instance_id)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
