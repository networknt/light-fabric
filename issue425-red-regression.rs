//! Issue #425 red regression. Only run against a fresh disposable database.
use chrono::{Duration, Utc};
use light_workflow::{
    artifact_publish::{ArtifactPublication, publish_artifact},
    artifact_retention::{ArtifactObjectStore, ArtifactRetentionReconciler},
    artifact_store::DurableArtifactStore,
    configuration::ArtifactSettings,
};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires fresh ARTIFACT_RETENTION_TEST_DATABASE_URL; never use a shared database"]
async fn cleanup_preserves_equal_bytes_referenced_by_retained_or_held_artifacts() {
    let url = std::env::var("ARTIFACT_RETENTION_TEST_DATABASE_URL")
        .expect("fresh disposable database URL required");
    let pool = PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    // Refuse any database which already has workflow state.
    sqlx::raw_sql("CREATE SCHEMA workflow_ops; CREATE ROLE operations_workflow_runtime; CREATE ROLE operations_workflow_migrator")
        .execute(&pool).await.unwrap();
    sqlx::raw_sql(workflow_store::MIGRATION_SQL)
        .execute(&pool)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = DurableArtifactStore::from_configuration(&ArtifactSettings {
        backend: "filesystem".into(),
        filesystem_root: Some(root.path().to_path_buf()),
        bucket: None,
        endpoint: None,
        allow_http: false,
        prefix: "issue425".into(),
        retention_days: 30,
    })
    .unwrap()
    .unwrap();
    let mut violations = Vec::new();
    for scenario in ["future-retention", "legal-hold", "process-deletion"] {
        let host = Uuid::now_v7();
        let expired = Uuid::now_v7();
        let survivor = Uuid::now_v7();
        let deleted_process = Uuid::now_v7();
        let bytes = b"identical requirements document";
        let mut digest = String::new();
        for artifact in [expired, survivor] {
            digest = publish_artifact(
                &pool,
                &store,
                ArtifactPublication {
                    host_id: host,
                    artifact_id: artifact,
                    execution_id: Uuid::now_v7(),
                    process_id: Some(if artifact == expired {
                        deleted_process
                    } else {
                        Uuid::now_v7()
                    }),
                    task_id: None,
                    logical_name: "requirements",
                    media_type: "text/plain",
                    producer: "issue425-regression",
                    policy_digest: "fixture",
                    retain_until: Utc::now() + Duration::days(30),
                    bytes,
                },
            )
            .await
            .unwrap();
        }
        let references: Vec<String> = sqlx::query_scalar("SELECT storage_reference FROM workflow_artifact_t WHERE host_id=$1 ORDER BY artifact_id")
            .bind(host).fetch_all(&pool).await.unwrap();
        // This assertion documents the legacy shared layout at the pinned HEAD.
        assert_eq!(references[0], references[1]);
        assert_eq!(
            store
                .read_verified(&host.to_string(), &digest, 1024)
                .await
                .unwrap(),
            bytes
        );
        if scenario == "legal-hold" {
            sqlx::query("UPDATE workflow_artifact_t SET legal_hold=true,retain_until_ts=now()-interval '1 hour' WHERE host_id=$1 AND artifact_id=$2")
                .bind(host).bind(survivor).execute(&pool).await.unwrap();
        }
        if scenario == "process-deletion" {
            // Direct helper invocation: mark_process_deleted has no production caller yet.
            assert_eq!(
                ArtifactRetentionReconciler::<DurableArtifactStore>::mark_process_deleted(
                    &pool,
                    host,
                    deleted_process,
                    "issue425-direct-test"
                )
                .await
                .unwrap(),
                1
            );
        } else {
            sqlx::query("UPDATE workflow_artifact_t SET retain_until_ts=now()-interval '1 hour' WHERE host_id=$1 AND artifact_id=$2")
                .bind(host).bind(expired).execute(&pool).await.unwrap();
        }
        assert_eq!(
            ArtifactRetentionReconciler::new(pool.clone(), store.clone(), 10)
                .reconcile_once()
                .await
                .unwrap(),
            1
        );
        let state: String = sqlx::query_scalar(
            "SELECT deletion_state FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(host)
        .bind(survivor)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, "RETAINED");
        let exists = store.exists(&references[0]).await.unwrap();
        let readable = store
            .read_verified(&host.to_string(), &digest, 1024)
            .await
            .is_ok_and(|read| read == bytes);
        eprintln!(
            "{scenario}: survivor_state={state}, object_exists={exists}, digest_verified_read={readable}"
        );
        if !exists || !readable {
            violations.push(scenario);
        }
    }
    pool.close().await;
    assert!(
        violations.is_empty(),
        "cleanup deleted bytes required by retained artifacts: {violations:?}"
    );
}
