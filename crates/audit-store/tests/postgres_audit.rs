use audit_store::{
    AuditClass, AuditRecord, ExpectedBinding, Repository, StoreError, sha256_digest,
};
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn audit_redaction_hold_erasure_isolation_and_restart_are_durable() {
    let Ok(database_url) = std::env::var("AUDIT_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_BINDING_ID").unwrap()).unwrap();
    let host_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_HOST_ID").unwrap()).unwrap();
    let other_host = Uuid::now_v7();
    let digest = std::env::var("PHASE6_TEST_BINDING_DIGEST").unwrap();
    let environment = std::env::var("PHASE6_TEST_ENVIRONMENT").unwrap();
    let pool = PgPoolOptions::new().connect(&database_url).await.unwrap();
    audit_store::validate(
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
        audit_store::validate(
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
    let subject = sha256_digest("user:phase6");
    let now = Utc::now();
    let safe_payload = json!({"decision":"DENY","statusCode":403,"contentRedacted":true});
    let record = AuditRecord {
        audit_id: Uuid::now_v7(),
        source_service: "light-gateway",
        source_instance: "gateway-phase6",
        event_type: "gateway.authorization.denied",
        event_class: AuditClass::Security,
        actor_digest: Some(&sha256_digest("actor")),
        subject_kind: Some("USER"),
        subject_digest: Some(&subject),
        correlation_digest: Some(&sha256_digest("correlation")),
        policy_digest: Some(&sha256_digest("policy")),
        redacted_payload: &safe_payload,
        occurred_at: now,
        retain_until: now + Duration::days(30),
        sink_profile_id: "tenant-audit-ca-v1",
    };
    repository.append(host_id, &record).await.unwrap();
    let prohibited = json!({"nested":{"authorization":"Bearer secret"}});
    let unsafe_record = AuditRecord {
        audit_id: Uuid::now_v7(),
        redacted_payload: &prohibited,
        ..record.clone()
    };
    assert!(matches!(
        repository.append(host_id, &unsafe_record).await,
        Err(StoreError::ProhibitedField(_))
    ));
    assert!(
        sqlx::query(
            "INSERT INTO audit_ops.audit_record_t(
               host_id,audit_id,source_service,source_instance,event_type,event_class,
               redacted_payload,evidence_digest,occurred_ts,retain_until_ts)
             VALUES($1,$2,'bypass','bypass','audit.bypass','SECURITY',
               '{\"authorization\":\"Bearer secret\"}'::jsonb,$3,now(),now()+interval '1 day')",
        )
        .bind(host_id)
        .bind(Uuid::now_v7())
        .bind(sha256_digest("unsafe-direct-insert"))
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(repository.export_host(other_host).await.unwrap().is_empty());

    let hold_id = Uuid::now_v7();
    repository
        .place_hold(host_id, hold_id, "USER", &subject, "LEGAL_REQUEST")
        .await
        .unwrap();
    assert!(matches!(
        repository
            .tombstone_subject(host_id, "USER", &subject, &sha256_digest("erasure"))
            .await,
        Err(StoreError::LegalHold)
    ));
    repository.release_hold(host_id, hold_id).await.unwrap();
    assert_eq!(
        repository
            .tombstone_subject(host_id, "USER", &subject, &sha256_digest("erasure"))
            .await
            .unwrap(),
        1
    );
    assert!(
        sqlx::query("UPDATE audit_ops.audit_record_t SET event_type='tampered' WHERE host_id=$1")
            .bind(host_id)
            .execute(&pool)
            .await
            .is_err()
    );
    pool.close().await;

    let restarted = PgPoolOptions::new().connect(&database_url).await.unwrap();
    let state: (i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM audit_ops.audit_record_t WHERE host_id=$1 AND erasure_state='TOMBSTONED'),
           (SELECT count(*) FROM audit_ops.audit_delivery_t WHERE host_id=$1)",
    )
    .bind(host_id)
    .fetch_one(&restarted)
    .await
    .unwrap();
    assert_eq!(state, (1, 1));
}
