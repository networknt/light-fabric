use artifact_store::{
    ArtifactRegistration, ExpectedBinding, Repository, StoreError, sha256_digest,
};
use chrono::{Duration, Utc};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn artifact_digest_scan_hold_tombstone_isolation_and_restart_are_durable() {
    let Ok(database_url) = std::env::var("ARTIFACT_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_BINDING_ID").unwrap()).unwrap();
    let host_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_HOST_ID").unwrap()).unwrap();
    let other_host = Uuid::now_v7();
    let digest = std::env::var("PHASE6_TEST_BINDING_DIGEST").unwrap();
    let environment = std::env::var("PHASE6_TEST_ENVIRONMENT").unwrap();
    let pool = PgPoolOptions::new().connect(&database_url).await.unwrap();
    artifact_store::validate(
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
    .unwrap();
    assert!(matches!(
        artifact_store::validate(
            &pool,
            &ExpectedBinding {
                binding_id,
                binding_digest: &digest,
                host_id: other_host,
                environment: &environment,
                minimum_schema_generation: 1,
            },
        )
        .await,
        Err(StoreError::Scope(_))
    ));
    let repository = Repository::new(pool.clone());
    let artifact_id = Uuid::now_v7();
    repository
        .register(
            host_id,
            &ArtifactRegistration {
                artifact_id,
                owner_service: "light-workflow",
                owner_kind: "TASK",
                owner_id: "task-phase6",
                logical_name: "result.json",
                media_type: "application/json",
                size_bytes: 2,
                content_digest: &sha256_digest("{}"),
                object_reference: "hosts/phase6/artifacts/result",
                visibility: "AUTHORIZED_CALLER",
                retain_until: Utc::now() - Duration::seconds(1),
                relationship_kind: "TASK",
                related_service: "light-workflow",
                related_id: "task-phase6",
            },
        )
        .await
        .unwrap();
    repository
        .record_scan(
            host_id,
            artifact_id,
            "CLEAN",
            "clamav-v1",
            &sha256_digest("scan-clean"),
        )
        .await
        .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO artifact_ops.artifact_metadata_t(
               host_id,artifact_id,owner_service,owner_kind,owner_id,logical_name,media_type,
               size_bytes,content_digest,object_reference,visibility,retain_until_ts)
             VALUES($1,$2,'bypass','UNBOUNDED_OWNER','owner','bad.json','application/json',
               2,$3,'hosts/phase6/artifacts/bad','OWNER',now()+interval '1 day')",
        )
        .bind(host_id)
        .bind(Uuid::now_v7())
        .bind(sha256_digest("{}"))
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(repository.export_host(other_host).await.unwrap().is_empty());
    let hold_id = Uuid::now_v7();
    repository
        .place_hold(host_id, artifact_id, hold_id, "LEGAL_REQUEST")
        .await
        .unwrap();
    assert!(matches!(
        repository
            .begin_deletion(host_id, artifact_id, Utc::now())
            .await,
        Err(StoreError::LegalHold)
    ));
    repository
        .release_hold(host_id, artifact_id, hold_id)
        .await
        .unwrap();
    repository
        .begin_deletion(host_id, artifact_id, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        repository
            .place_hold(host_id, artifact_id, Uuid::now_v7(), "TOO_LATE")
            .await,
        Err(StoreError::Scope(_))
    ));
    repository
        .tombstone(host_id, artifact_id, &sha256_digest("deleted"), Utc::now())
        .await
        .unwrap();
    assert!(
        sqlx::query(
            "UPDATE artifact_ops.artifact_metadata_t SET content_digest=$3
             WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(sha256_digest("tampered"))
        .execute(&pool)
        .await
        .is_err()
    );
    pool.close().await;

    let restarted = PgPoolOptions::new().connect(&database_url).await.unwrap();
    let state: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM artifact_ops.artifact_metadata_t WHERE host_id=$1 AND lifecycle_state='TOMBSTONED'),
           (SELECT count(*) FROM artifact_ops.artifact_relationship_t WHERE host_id=$1),
           (SELECT count(*) FROM artifact_ops.artifact_event_t WHERE host_id=$1)",
    )
    .bind(host_id)
    .fetch_one(&restarted)
    .await
    .unwrap();
    assert_eq!(state.0, 1);
    assert_eq!(state.1, 1);
    assert!(state.2 >= 5);
}
