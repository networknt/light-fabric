use axum::{Json, Router, routing::post};
use chrono::Utc;
use gateway_operational_store::{
    AdmissionOutcome, EvidenceClass, EvidenceRecord, ExpectedBinding, HttpPublisher, Repository,
    SpoolLimits, StoreError, sha256_digest,
};
use sqlx::postgres::PgPoolOptions;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use uuid::Uuid;

fn record(class: EvidenceClass, endpoint: &str) -> EvidenceRecord {
    EvidenceRecord {
        event_id: Uuid::now_v7(),
        event_class: class,
        event_type: "gateway.request.completed".into(),
        method: "POST".into(),
        endpoint: endpoint.into(),
        status_code: 200,
        duration_micros: 25,
        request_bytes: 12,
        response_bytes: 24,
        correlation_digest: Some(sha256_digest("correlation")),
        principal_digest: Some(sha256_digest("principal")),
        policy_digest: Some(sha256_digest("policy")),
        handler_digest: Some(sha256_digest("handler")),
        occurred_at: Utc::now(),
    }
}

#[tokio::test]
async fn bounded_spool_survives_sink_failure_restart_and_http_delivery() {
    let Ok(database_url) = std::env::var("GATEWAY_STORE_TEST_DATABASE_URL") else {
        return;
    };
    let binding_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_BINDING_ID").unwrap()).unwrap();
    let host_id = Uuid::parse_str(&std::env::var("PHASE6_TEST_HOST_ID").unwrap()).unwrap();
    let digest = std::env::var("PHASE6_TEST_BINDING_DIGEST").unwrap();
    let environment = std::env::var("PHASE6_TEST_ENVIRONMENT").unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await
        .unwrap();
    gateway_operational_store::validate(
        &pool,
        &ExpectedBinding {
            binding_id,
            binding_digest: &digest,
            host_id,
            environment: &environment,
            server_host: "postgres",
            port: 5432,
            tls_mode: "DISABLE",
            expected_database: "operations",
            minimum_schema_generation: 1,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        gateway_operational_store::validate(
            &pool,
            &ExpectedBinding {
                binding_id,
                binding_digest: &digest,
                host_id: Uuid::now_v7(),
                environment: &environment,
                server_host: "postgres",
                port: 5432,
                tls_mode: "DISABLE",
                expected_database: "operations",
                minimum_schema_generation: 1,
            },
        )
        .await,
        Err(_)
    ));

    let repository = Repository::new(
        pool.clone(),
        host_id,
        "gateway-phase6",
        SpoolLimits {
            maximum_pending_records: 2,
            maximum_pending_bytes: 32_768,
        },
    )
    .unwrap();
    assert_eq!(
        repository
            .record(&record(EvidenceClass::RequiredAudit, "/v1/accounts/{id}"))
            .await
            .unwrap(),
        AdmissionOutcome::Persisted
    );
    assert_eq!(
        repository
            .record(&record(EvidenceClass::Traffic, "/v1/accounts/{id}"))
            .await
            .unwrap(),
        AdmissionOutcome::Persisted
    );
    assert_eq!(
        repository
            .record(&record(EvidenceClass::Traffic, "/v1/accounts/{id}"))
            .await
            .unwrap(),
        AdmissionOutcome::DroppedOptional
    );
    assert!(matches!(
        repository
            .record(&record(EvidenceClass::RequiredAudit, "/v1/accounts/{id}"))
            .await,
        Err(StoreError::SpoolFull)
    ));

    let claimed = repository
        .claim("phase6-publisher", 10, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    let received = Arc::new(AtomicUsize::new(0));
    let receiver = Arc::clone(&received);
    let app = Router::new().route(
        "/ingest",
        post(move |Json(records): Json<Vec<serde_json::Value>>| {
            let receiver = Arc::clone(&receiver);
            async move {
                receiver.fetch_add(records.len(), Ordering::SeqCst);
                axum::http::StatusCode::ACCEPTED
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    HttpPublisher::new(format!("http://{address}/ingest"), None)
        .unwrap()
        .publish(&claimed)
        .await
        .unwrap();
    repository.delivered(&claimed).await.unwrap();
    assert_eq!(received.load(Ordering::SeqCst), 2);
    server.abort();

    repository
        .record(&record(EvidenceClass::RequiredAudit, "/v1/secure"))
        .await
        .unwrap();
    let outage = repository
        .claim("phase6-publisher", 10, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(outage.len(), 1);
    assert!(
        HttpPublisher::new("http://127.0.0.1:9/ingest", None)
            .unwrap()
            .publish(&outage)
            .await
            .is_err()
    );
    repository
        .retry(&outage, "sink_unavailable", Duration::ZERO)
        .await
        .unwrap();
    let stale = repository
        .claim("phase6-publisher-stale", 10, Duration::ZERO)
        .await
        .unwrap();
    let current = repository
        .claim("phase6-publisher-current", 10, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(matches!(
        repository.delivered(&stale).await,
        Err(StoreError::Scope(_))
    ));
    repository
        .retry(&current, "restart_exercise", Duration::ZERO)
        .await
        .unwrap();
    pool.close().await;

    let restarted = PgPoolOptions::new().connect(&database_url).await.unwrap();
    let state: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM gateway_ops.gateway_evidence_spool_t WHERE state='PENDING'),
           (SELECT pending_records FROM gateway_ops.gateway_evidence_quota_t WHERE host_id=$1),
           (SELECT dropped_optional_records FROM gateway_ops.gateway_evidence_quota_t WHERE host_id=$1)",
    )
    .bind(host_id)
    .fetch_one(&restarted)
    .await
    .unwrap();
    assert_eq!(state, (1, 1, 1));
}
