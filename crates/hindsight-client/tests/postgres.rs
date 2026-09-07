use hindsight_client::{HindsightMemory, PgHindsightClient};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires HINDSIGHT_TEST_DATABASE_URL with public.vector installed"]
async fn retain_recall_with_restricted_search_path() -> anyhow::Result<()> {
    let url = std::env::var("HINDSIGHT_TEST_DATABASE_URL")
        .expect("set HINDSIGHT_TEST_DATABASE_URL to run the PostgreSQL test");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO agent_ops, operational_meta")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    // A session-local fixture shadows any real table without touching its rows.
    // One pooled connection keeps the fixture and all client calls in this session.
    sqlx::query(
        "CREATE TEMP TABLE agent_memory_unit_t (
            host_id uuid NOT NULL, unit_id uuid PRIMARY KEY, bank_id uuid NOT NULL,
            content text NOT NULL, fact_type text NOT NULL,
            embedding public.vector(3), metadata jsonb NOT NULL)",
    )
    .execute(&pool)
    .await?;
    let visible: bool = sqlx::query_scalar("SELECT to_regtype('vector') IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    assert!(!visible, "test must not expose public through search_path");
    let client = PgHindsightClient::new(pool.clone());
    let host = Uuid::new_v4();
    let bank = Uuid::new_v4();
    let metadata = serde_json::json!({"source": "regression"});
    let near = client
        .retain(
            host,
            bank,
            "near",
            "Fact",
            Some(vec![1., 0., 0.]),
            metadata.clone(),
        )
        .await?;
    let far = client
        .retain(
            host,
            bank,
            "far",
            "Fact",
            Some(vec![0., 1., 0.]),
            metadata.clone(),
        )
        .await?;
    let missing = client
        .retain(host, bank, "missing", "Fact", None, metadata.clone())
        .await?;
    client
        .retain(
            Uuid::new_v4(),
            bank,
            "other host",
            "Fact",
            Some(vec![1., 0., 0.]),
            metadata.clone(),
        )
        .await?;
    client
        .retain(
            host,
            Uuid::new_v4(),
            "other bank",
            "Fact",
            Some(vec![1., 0., 0.]),
            metadata.clone(),
        )
        .await?;
    let rows = client.recall(host, bank, vec![1., 0., 0.], 2).await?;
    assert_eq!(
        rows.iter().map(|row| row.unit_id).collect::<Vec<_>>(),
        [near, far]
    );
    assert_eq!(rows[0].metadata, metadata);
    assert_eq!(rows[0].content, "near");
    let rows = client.recall(host, bank, vec![1., 0., 0.], 10).await?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].unit_id, missing);
    pool.close().await;
    Ok(())
}
