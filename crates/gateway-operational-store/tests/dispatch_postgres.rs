use chrono::Utc;
use gateway_operational_store::{
    AdmissionOutcome, DispatchObservation, DispatchPhase, EvidenceClass, EvidenceRecord,
    Repository, SpoolLimits, StoreError, sha256_digest,
};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn observed(audit: Uuid, phase: DispatchPhase, sequence: u32) -> EvidenceRecord {
    EvidenceRecord {
        event_id: Uuid::now_v7(),
        event_class: EvidenceClass::RequiredAudit,
        event_type: match phase {
            DispatchPhase::Started => "gateway.dispatch.observation.started",
            DispatchPhase::Terminal => "gateway.dispatch.observation.terminal",
            _ => unreachable!(),
        }
        .into(),
        method: "GET".into(),
        endpoint: "/github/repos/*@get".into(),
        status_code: 403,
        duration_micros: 0,
        request_bytes: 0,
        response_bytes: 0,
        correlation_digest: Some(sha256_digest(&audit.to_string())),
        principal_digest: None,
        policy_digest: None,
        handler_digest: None,
        occurred_at: Utc::now(),
        dispatch_observation: Some(DispatchObservation {
            request_audit_id: audit,
            dispatch_phase: phase,
            dispatch_sequence: sequence,
            upstream_attempt_count: 0,
            upstream_handoff_count: 0,
            observation_complete: phase == DispatchPhase::Terminal,
            deployment_config_digest: sha256_digest("local-identity"),
            observer_contract_version: 1,
            completion_phase: if phase == DispatchPhase::Terminal {
                "response"
            } else {
                "in_progress"
            }
            .into(),
        }),
    }
}

#[tokio::test]
#[ignore = "requires disposable G03 PostgreSQL fixture"]
async fn dispatch_capacity_reclaim_legacy_compatibility_and_atomic_durability() {
    let port = std::env::var("POLICY_ADMIN_TEST_PORT").unwrap();
    let pool=sqlx::PgPool::connect(&format!("postgres://operations_gateway_runtime:localfixture@127.0.0.1:{port}/operations?sslmode=disable&options=-csearch_path%3Dgateway_ops%2Coperational_meta")).await.unwrap();
    let repository = Repository::new(
        pool.clone(),
        Uuid::now_v7(),
        "g03-capacity",
        SpoolLimits {
            maximum_pending_records: 2,
            maximum_pending_bytes: 32768,
        },
    )
    .unwrap();
    let audit = Uuid::now_v7();
    let start = observed(audit, DispatchPhase::Started, 0);
    let terminal = observed(audit, DispatchPhase::Terminal, 1);
    let mut times = Vec::new();
    for record in [&start, &terminal] {
        let began = Instant::now();
        assert_eq!(
            repository.record(record).await.unwrap(),
            AdmissionOutcome::Persisted
        );
        times.push(began.elapsed().as_micros());
    }
    assert!(matches!(
        repository
            .record(&observed(Uuid::now_v7(), DispatchPhase::Started, 0))
            .await,
        Err(StoreError::SpoolFull)
    ));
    let claimed = repository
        .claim("isolated", 4, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(claimed.iter().all(|r| r.dispatch_observation.is_some()));
    repository.delivered(&claimed).await.unwrap();
    // Delivery acknowledgement retains phase rows until explicit retention expiry.
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM gateway_evidence_spool_t WHERE gateway_instance='g03-capacity' AND dispatch_observation IS NOT NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);
    let mut legacy = start.clone();
    legacy.event_id = Uuid::now_v7();
    legacy.event_class = EvidenceClass::Traffic;
    legacy.event_type = "gateway.request.completed".into();
    legacy.dispatch_observation = None;
    repository.record(&legacy).await.unwrap();
    let rows = repository
        .claim("legacy", 4, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].dispatch_observation.is_none());
    // Invalid sequences and extra/untyped metadata are rejected atomically.
    let mut invalid = observed(Uuid::now_v7(), DispatchPhase::Terminal, 5);
    assert!(matches!(
        repository.record(&invalid).await,
        Err(StoreError::Scope(_))
    ));
    invalid
        .dispatch_observation
        .as_mut()
        .unwrap()
        .dispatch_sequence = 1;
    invalid
        .dispatch_observation
        .as_mut()
        .unwrap()
        .upstream_attempt_count = u32::MAX;
    assert!(matches!(
        repository.record(&invalid).await,
        Err(StoreError::Scope(_))
    ));
    if let Ok(path) = std::env::var("G03_STORE_MEASUREMENT_FILE") {
        std::fs::write(path,serde_json::to_vec_pretty(&serde_json::json!({"durableTransactionMicros":times,"samples":2,"isolatedPostgres":true,"includesFirstQuotaInsert":true,"productionLatencyClaim":false})).unwrap()).unwrap();
    }
}
