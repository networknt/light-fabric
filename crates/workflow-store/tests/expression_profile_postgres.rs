//! Owner-run database contract gate; ordinary Cargo runs never connect.
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires explicitly owned, empty W2-migrated E04_W2_TEST_DATABASE_URL"]
async fn expression_profile_claim_and_consistency_contract() {
    let url = std::env::var("E04_W2_TEST_DATABASE_URL").expect("owned scratch database URL");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("scratch connection");
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        database.starts_with("e04_w2_"),
        "dedicated scratch database required"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_ops.process_info_t")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "empty fixture database required");
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("expression_profile_schema_gate.sql"))
        .execute(&mut *tx)
        .await
        .expect("claim and consistency contract");
    tx.rollback().await.unwrap();
}
