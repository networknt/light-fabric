//! Issue #425: real PostgreSQL metadata and disposable filesystem bytes.
//! Each test requires its own fresh database, suffixed with the test case name.
//! Late operations below are deterministic local simulations, not S3 evidence.
use async_trait::async_trait;
use chrono::{Duration, Utc};
use light_workflow::{
    artifact_publish::{
        ArtifactPublication, ArtifactPublisherStore, promote_artifact_evidence, publish_artifact,
        publish_artifact_in_transaction,
    },
    artifact_retention::{
        ArtifactDeletionPolicy, ArtifactObjectStore, ArtifactRetentionReconciler,
        ArtifactStoreError,
    },
    artifact_store::DurableArtifactStore,
    configuration::ArtifactSettings,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

const BYTES: &[u8] = b"identical requirements document";
struct Fixture {
    pool: PgPool,
    store: DurableArtifactStore,
    root: tempfile::TempDir,
    host: Uuid,
}
impl Fixture {
    async fn new(case: &str) -> Self {
        let raw = std::env::var("ARTIFACT_RETENTION_TEST_DATABASE_URL")
            .expect("fresh disposable database prefix URL required");
        let mut url = url::Url::parse(&raw).unwrap();
        assert!(
            matches!(url.host_str(), Some("127.0.0.1") | Some("localhost")),
            "fixture must be local"
        );
        let base = url.path().trim_start_matches('/');
        assert!(
            base.starts_with("issue425_"),
            "dedicated issue425 database prefix required"
        );
        url.set_path(&format!("/{base}_{case}"));
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url.as_str())
            .await
            .unwrap();
        // Refuse an already-used database. Roles are provisioned only in the disposable container.
        sqlx::raw_sql("CREATE SCHEMA workflow_ops")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(workflow_store::MIGRATION_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(workflow_store::ARTIFACT_RETIREMENT_MIGRATION_SQL)
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
        Self {
            pool,
            store,
            root,
            host: Uuid::now_v7(),
        }
    }
    fn publication(&self, id: Uuid) -> ArtifactPublication<'_> {
        ArtifactPublication {
            host_id: self.host,
            artifact_id: id,
            execution_id: id,
            process_id: Some(id),
            task_id: None,
            logical_name: "requirements",
            media_type: "text/plain",
            producer: "issue425-regression",
            policy_digest: "fixture",
            retain_until: Utc::now() + Duration::days(30),
            bytes: BYTES,
        }
    }
    async fn publish(&self, id: Uuid) -> String {
        publish_artifact(&self.pool, &self.store, self.publication(id))
            .await
            .unwrap();
        self.reference(id).await
    }
    async fn reference(&self, id: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT storage_reference FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(self.host)
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
    async fn expire(&self, id: Uuid) {
        sqlx::query("UPDATE workflow_artifact_t SET retain_until_ts=now()-interval '1 hour' WHERE host_id=$1 AND artifact_id=$2")
            .bind(self.host).bind(id).execute(&self.pool).await.unwrap();
    }
    async fn read(&self, id: Uuid) {
        assert_eq!(
            self.store
                .read_verified(
                    &self.host.to_string(),
                    id,
                    &self.reference(id).await,
                    &digest(),
                    1024
                )
                .await
                .unwrap(),
            BYTES
        );
    }
    async fn state(&self, id: Uuid) -> (String, serde_json::Value) {
        sqlx::query_as("SELECT deletion_state,COALESCE(deletion_evidence,'{}'::jsonb) FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2")
            .bind(self.host).bind(id).fetch_one(&self.pool).await.unwrap()
    }
    async fn cleanup(&self) -> u64 {
        ArtifactRetentionReconciler::new(self.pool.clone(), self.store.clone(), 10)
            .reconcile_once()
            .await
            .unwrap()
    }
    async fn legacy(&self, ids: &[Uuid]) -> String {
        // Materialize the actual old shared layout, independent of new promotion.
        let hash = hex::encode(Sha256::digest(BYTES));
        let key = format!(
            "issue425/tenants/{}/objects/sha256/{}/{hash}",
            self.host,
            &hash[..2]
        );
        let path = self.root.path().join(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, BYTES).unwrap();
        let reference = format!("object://{key}");
        for id in ids {
            self.publish(*id).await;
            sqlx::query("UPDATE workflow_artifact_t SET storage_reference=$3 WHERE host_id=$1 AND artifact_id=$2")
                .bind(self.host).bind(id).bind(&reference).execute(&self.pool).await.unwrap();
        }
        reference
    }
}
fn digest() -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(BYTES)))
}

async fn legacy_case(case: &str) {
    let f = Fixture::new(case).await;
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let reference = f.legacy(&[a, b]).await;
    // Explicit equality remains mandatory legacy coverage.
    assert_eq!(f.reference(a).await, f.reference(b).await);
    f.read(b).await;
    if case == "hold" {
        f.expire(b).await;
        sqlx::query(
            "UPDATE workflow_artifact_t SET legal_hold=true WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(f.host)
        .bind(b)
        .execute(&f.pool)
        .await
        .unwrap();
    }
    if case == "process" {
        // Direct helper test, not a production deletion flow.
        assert_eq!(
            ArtifactRetentionReconciler::<DurableArtifactStore>::mark_process_deleted(
                &f.pool,
                f.host,
                a,
                "direct-test"
            )
            .await
            .unwrap(),
            1
        );
    } else {
        f.expire(a).await;
    }
    assert_eq!(f.cleanup().await, 1);
    let (state, evidence) = f.state(a).await;
    assert_eq!(state, "RETIRED");
    assert_eq!(evidence["physicalDeleteDeferred"], true);
    assert!(evidence.get("verifiedAbsent").is_none());
    assert_eq!(f.state(b).await.0, "RETAINED");
    assert!(f.store.exists(&reference).await.unwrap());
    f.read(b).await;
    // Live legacy replay keeps its stored binding; no silent migration.
    assert_eq!(f.publish(b).await, reference);
    if case != "hold" {
        f.expire(b).await;
        assert_eq!(f.cleanup().await, 1);
        assert_eq!(f.state(b).await.0, "RETIRED");
        assert!(f.store.exists(&reference).await.unwrap());
    }
    // A stale legacy claim also retires without physical deletion.
    sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETING',updated_ts=now()-interval '6 minutes' WHERE host_id=$1 AND artifact_id=$2")
        .bind(f.host).bind(a).execute(&f.pool).await.unwrap();
    let r = ArtifactRetentionReconciler::new(f.pool.clone(), f.store.clone(), 10);
    assert_eq!(r.requeue_stale().await.unwrap(), 1);
    assert_eq!(r.reconcile_once().await.unwrap(), 1);
    assert!(f.store.exists(&reference).await.unwrap());
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn legacy_retained_sibling() {
    legacy_case("retained").await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn legacy_legal_hold_sibling() {
    legacy_case("hold").await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn legacy_direct_process_helper() {
    legacy_case("process").await;
}

#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn new_keys_retry_and_retired_identity_fence() {
    let f = Fixture::new("identity").await;
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let ar = f.publish(a).await;
    let br = f.publish(b).await;
    f.expire(b).await;
    sqlx::query(
        "UPDATE workflow_artifact_t SET legal_hold=true WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(f.host)
    .bind(b)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_ne!(ar, br);
    assert_eq!(f.publish(a).await, ar);
    let mut changed = f.publication(a);
    changed.process_id = Some(b);
    assert!(publish_artifact(&f.pool, &f.store, changed).await.is_err());
    for state in [
        "DELETE_PENDING",
        "DELETING",
        "DELETE_FAILED",
        "DELETED",
        "RETIRED",
    ] {
        sqlx::query(
            "UPDATE workflow_artifact_t SET deletion_state=$3 WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(f.host)
        .bind(a)
        .bind(state)
        .execute(&f.pool)
        .await
        .unwrap();
        assert!(
            publish_artifact(&f.pool, &f.store, f.publication(a))
                .await
                .is_err()
        );
        let mut tx = f.pool.begin().await.unwrap();
        assert!(
            publish_artifact_in_transaction(&mut tx, &f.store, f.publication(a))
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }
    sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETE_PENDING',deletion_next_retry_ts=now() WHERE host_id=$1 AND artifact_id=$2")
        .bind(f.host).bind(a).execute(&f.pool).await.unwrap();
    assert_eq!(f.cleanup().await, 1);
    assert_eq!(f.state(a).await.0, "DELETED");
    assert_eq!(f.state(a).await.1["verifiedAbsent"], true);
    assert!(!f.store.exists(&ar).await.unwrap());
    f.read(b).await;
    // A tampered row cannot turn its cleanup into another artifact's delete.
    let tampered = Uuid::now_v7();
    f.publish(tampered).await;
    sqlx::query("UPDATE workflow_artifact_t SET storage_reference=$3,retain_until_ts=now()-interval '1 hour' WHERE host_id=$1 AND artifact_id=$2")
        .bind(f.host).bind(tampered).bind(&br).execute(&f.pool).await.unwrap();
    assert_eq!(f.cleanup().await, 1);
    assert_eq!(f.state(tampered).await.0, "DELETE_FAILED");
    f.read(b).await;
    // Tenant isolation uses the same ID and digest, so only the Host differs.
    let other = Uuid::now_v7();
    let mut p = f.publication(a);
    p.host_id = other;
    publish_artifact(&f.pool, &f.store, p).await.unwrap();
    let other_ref: String = sqlx::query_scalar(
        "SELECT storage_reference FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(other)
    .bind(a)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_ne!(ar, other_ref);
    assert_eq!(
        f.store
            .read_verified(&other.to_string(), a, &other_ref, &digest(), 1024)
            .await
            .unwrap(),
        BYTES
    );
}

#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn runner_replay_rejects_deleted_and_changed_authority() {
    let f = Fixture::new("runner").await;
    let execution = Uuid::now_v7();
    let process = Uuid::now_v7();
    let task = Uuid::now_v7();
    let staging = f
        .store
        .stage(&format!("{}/{execution}/runner", f.host), BYTES)
        .await
        .unwrap();
    let evidence = execution_runner_protocol::ArtifactEvidence {
        logical_name: "requirements".into(),
        file_type: "text".into(),
        media_type: "text/plain".into(),
        size: BYTES.len() as u64,
        digest: digest(),
        reference: staging,
    };
    promote_artifact_evidence(
        &f.pool,
        &f.store,
        f.host,
        execution,
        process,
        task,
        "fixture",
        Utc::now() + Duration::days(1),
        &evidence,
    )
    .await
    .unwrap();
    promote_artifact_evidence(
        &f.pool,
        &f.store,
        f.host,
        execution,
        process,
        task,
        "fixture",
        Utc::now() + Duration::days(1),
        &evidence,
    )
    .await
    .unwrap();
    assert!(
        promote_artifact_evidence(
            &f.pool,
            &f.store,
            f.host,
            execution,
            Uuid::now_v7(),
            task,
            "fixture",
            Utc::now() + Duration::days(1),
            &evidence
        )
        .await
        .is_err()
    );
    for state in [
        "DELETE_PENDING",
        "DELETING",
        "DELETE_FAILED",
        "DELETED",
        "RETIRED",
    ] {
        sqlx::query("UPDATE workflow_artifact_t SET deletion_state=$2 WHERE host_id=$1")
            .bind(f.host)
            .bind(state)
            .execute(&f.pool)
            .await
            .unwrap();
        assert!(
            promote_artifact_evidence(
                &f.pool,
                &f.store,
                f.host,
                execution,
                process,
                task,
                "fixture",
                Utc::now() + Duration::days(1),
                &evidence
            )
            .await
            .is_err()
        );
    }
}

// The first local DELETE may execute much later. Semaphore handshakes avoid timing sleeps.
#[derive(Clone)]
struct LateStore {
    inner: DurableArtifactStore,
    calls: Arc<AtomicUsize>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    fail_late: bool,
    release_retry: Arc<Semaphore>,
}
#[async_trait]
impl ArtifactObjectStore for LateStore {
    fn deletion_policy(
        &self,
        h: Uuid,
        a: Uuid,
        r: &str,
        d: &str,
    ) -> Result<ArtifactDeletionPolicy, ArtifactStoreError> {
        self.inner.deletion_policy(h, a, r, d)
    }
    async fn delete(&self, r: &str) -> Result<(), ArtifactStoreError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.inner.delete(r).await?;
            if self.fail_late {
                return Err(ArtifactStoreError {
                    message: "simulated lost reply after late delete".into(),
                    retryable: true,
                });
            }
        } else {
            self.entered.add_permits(1);
            self.release_retry.acquire().await.unwrap().forget();
            self.inner.delete(r).await?;
        }
        Ok(())
    }
    async fn exists(&self, r: &str) -> Result<bool, ArtifactStoreError> {
        self.inner.exists(r).await
    }
}
async fn late_case(case: &str, fail_late: bool) {
    let f = Fixture::new(case).await;
    let old = Uuid::now_v7();
    let old_ref = f.publish(old).await;
    f.expire(old).await;
    let store = LateStore {
        inner: f.store.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
        entered: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
        fail_late,
        release_retry: Arc::new(Semaphore::new(0)),
    };
    let worker = ArtifactRetentionReconciler::new(f.pool.clone(), store.clone(), 10);
    let first = tokio::spawn(async move { worker.reconcile_once().await.unwrap() });
    store.entered.acquire().await.unwrap().forget();
    sqlx::query("UPDATE workflow_artifact_t SET updated_ts=now()-interval '6 minutes' WHERE host_id=$1 AND artifact_id=$2").bind(f.host).bind(old).execute(&f.pool).await.unwrap();
    let retry = ArtifactRetentionReconciler::new(f.pool.clone(), store.clone(), 10);
    assert_eq!(retry.requeue_stale().await.unwrap(), 1);
    assert!(
        publish_artifact(&f.pool, &f.store, f.publication(old))
            .await
            .is_err()
    );
    let fresh = Uuid::now_v7();
    let fresh_ref = f.publish(fresh).await;
    assert_ne!(old_ref, fresh_ref);
    // Attempt two remains DELETING while stale attempt one completes.
    let second = tokio::spawn(async move { retry.reconcile_once().await.unwrap() });
    store.entered.acquire().await.unwrap().forget();
    let before = f.state(old).await;
    assert_eq!(before.0, "DELETING");
    store.release.add_permits(1);
    assert_eq!(first.await.unwrap(), 1);
    assert_eq!(
        f.state(old).await,
        before,
        "stale completion cannot mutate a newer active attempt"
    );
    f.read(fresh).await;
    store.release_retry.add_permits(1);
    assert_eq!(second.await.unwrap(), 1);
    let after = f.state(old).await;
    assert_eq!(after.0, "DELETED");
    assert_eq!(after.1["attempt"], 2);
    f.read(fresh).await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn stale_success_and_late_delete() {
    late_case("late_success", false).await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn stale_failure_and_late_delete() {
    late_case("late_failure", true).await;
}

#[derive(Clone)]
struct BlockPromotion {
    inner: DurableArtifactStore,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}
#[async_trait]
impl ArtifactPublisherStore for BlockPromotion {
    async fn stage(&self, k: &str, b: &[u8]) -> Result<String, ArtifactStoreError> {
        self.inner.stage(k, b).await
    }
    async fn promote(
        &self,
        n: &str,
        a: Uuid,
        s: &str,
        d: &str,
    ) -> Result<String, ArtifactStoreError> {
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        self.inner.promote(n, a, s, d).await
    }
    async fn verify_bound(
        &self,
        n: &str,
        a: Uuid,
        r: &str,
        d: &str,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.verify_bound(n, a, r, d).await
    }
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn concurrent_publication_and_cleanup() {
    let f = Fixture::new("concurrent").await;
    let id = Uuid::now_v7();
    let store = BlockPromotion {
        inner: f.store.clone(),
        entered: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    };
    // Existing pending promotion makes its row visible while the publisher owns its lock.
    let staging = f
        .store
        .stage(&format!("{}/{id}/{id}", f.host), BYTES)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,process_id,logical_name,media_type,size_bytes,content_digest,storage_reference,staging_reference,promotion_state,producer,policy_digest,retain_until_ts,verification_state) VALUES($1,$2,$2,$2,'requirements','text/plain',$3,$4,$5,$5,'METADATA_COMMITTED','issue425-regression','fixture',now()+interval '1 day','PENDING')")
        .bind(f.host).bind(id).bind(BYTES.len() as i64).bind(digest()).bind(staging).execute(&f.pool).await.unwrap();
    let pool = f.pool.clone();
    let blocked = store.clone();
    let host = f.host;
    let publisher = tokio::spawn(async move {
        publish_artifact(
            &pool,
            &blocked,
            ArtifactPublication {
                host_id: host,
                artifact_id: id,
                execution_id: id,
                process_id: Some(id),
                task_id: None,
                logical_name: "requirements",
                media_type: "text/plain",
                producer: "issue425-regression",
                policy_digest: "fixture",
                retain_until: Utc::now() + Duration::days(1),
                bytes: BYTES,
            },
        )
        .await
        .unwrap()
    });
    store.entered.acquire().await.unwrap().forget();
    // A process helper update must block on the publisher's row lock.
    let pool = f.pool.clone();
    let pending = tokio::spawn(async move {
        ArtifactRetentionReconciler::<DurableArtifactStore>::mark_process_deleted(
            &pool,
            host,
            id,
            "concurrent-helper",
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'UPDATE workflow_artifact_t SET deletion_state=%')")
                .fetch_one(&f.pool).await.unwrap();
            if waiting { break; }
            assert!(!pending.is_finished(), "process deletion escaped the publication lock");
            tokio::task::yield_now().await;
        }
    }).await.expect("observe actual PostgreSQL lock wait");
    assert_eq!(f.cleanup().await, 0);
    store.release.add_permits(1);
    publisher.await.unwrap();
    assert_eq!(pending.await.unwrap(), 1);
    f.read(id).await;
    assert_eq!(f.cleanup().await, 1);
    assert_eq!(f.state(id).await.0, "DELETED");
    assert!(
        publish_artifact(&f.pool, &f.store, f.publication(id))
            .await
            .is_err()
    );
}

async fn interrupted_case(case: &str, missing: bool, foreign_reference: bool) {
    let f = Fixture::new(case).await;
    let id = Uuid::now_v7();
    let protected = Uuid::now_v7();
    let protected_ref = f.publish(protected).await;
    let staging = f
        .store
        .stage(&format!("{}/{id}/{id}", f.host), BYTES)
        .await
        .unwrap();
    if missing {
        f.store.delete(&staging).await.unwrap();
    }
    let reference = if foreign_reference {
        &protected_ref
    } else {
        &staging
    };
    // A persisted legacy METADATA_COMMITTED row, not a BOUND artifact rewritten by a publisher.
    sqlx::query("INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,process_id,logical_name,media_type,size_bytes,content_digest,storage_reference,staging_reference,promotion_state,producer,policy_digest,retain_until_ts,verification_state) VALUES($1,$2,$2,$2,'requirements','text/plain',$3,$4,$5,$5,'METADATA_COMMITTED','issue425-regression','fixture',now()-interval '1 hour','PENDING')")
        .bind(f.host).bind(id).bind(BYTES.len() as i64).bind(digest()).bind(reference).execute(&f.pool).await.unwrap();
    assert_eq!(f.cleanup().await, 1);
    let (state, evidence) = f.state(id).await;
    assert_eq!(
        state, "RETIRED",
        "unbound rows must terminate, not retry forever"
    );
    assert_eq!(evidence["reason"], "unbound-artifact");
    assert_eq!(evidence["promotionState"], "METADATA_COMMITTED");
    assert_eq!(evidence["physicalDeleteDeferred"], true);
    assert!(evidence.get("verifiedAbsent").is_none());
    assert_eq!(f.cleanup().await, 0);
    assert_eq!(f.store.exists(&staging).await.unwrap(), !missing);
    f.read(protected).await;
    // Terminal retirement fences recovery even if staged bytes reappear.
    assert!(
        publish_artifact(&f.pool, &f.store, f.publication(id))
            .await
            .is_err()
    );
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn interrupted_promotion_retires_without_deleting_staging() {
    interrupted_case("unbound_present", false, false).await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn interrupted_promotion_with_missing_staging_terminates() {
    interrupted_case("unbound_missing", true, false).await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn unbound_reference_never_authorizes_another_artifacts_delete() {
    interrupted_case("unbound_foreign", false, true).await;
}
#[tokio::test]
#[ignore = "fresh disposable issue425 databases required"]
async fn permanent_runner_promotion_failure_retires_truthfully() {
    let f = Fixture::new("quarantine").await;
    let execution = Uuid::now_v7();
    let process = Uuid::now_v7();
    let task = Uuid::now_v7();
    let staging = f
        .store
        .stage(&format!("{}/{execution}/runner", f.host), b"wrong bytes")
        .await
        .unwrap();
    let evidence = execution_runner_protocol::ArtifactEvidence {
        logical_name: "requirements".into(),
        file_type: "text".into(),
        media_type: "text/plain".into(),
        size: BYTES.len() as u64,
        digest: digest(),
        reference: staging.clone(),
    };
    assert!(
        promote_artifact_evidence(
            &f.pool,
            &f.store,
            f.host,
            execution,
            process,
            task,
            "fixture",
            Utc::now() + Duration::days(1),
            &evidence
        )
        .await
        .is_err()
    );
    let (id, promotion, verification): (Uuid, String, String) = sqlx::query_as("SELECT artifact_id,promotion_state,verification_state FROM workflow_artifact_t WHERE host_id=$1")
        .bind(f.host).fetch_one(&f.pool).await.unwrap();
    assert_eq!(promotion, "QUARANTINED");
    assert_eq!(verification, "REJECTED");
    f.expire(id).await;
    // Legal hold applies to failed/unbound evidence too.
    sqlx::query(
        "UPDATE workflow_artifact_t SET legal_hold=true WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(f.host)
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(f.cleanup().await, 0);
    sqlx::query(
        "UPDATE workflow_artifact_t SET legal_hold=false WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(f.host)
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(f.cleanup().await, 1);
    let (state, cleanup) = f.state(id).await;
    assert_eq!(state, "RETIRED");
    assert_eq!(cleanup["reason"], "unbound-artifact");
    assert_eq!(cleanup["promotionState"], "QUARANTINED");
    assert_eq!(cleanup["physicalDeleteDeferred"], true);
    assert!(cleanup.get("verifiedAbsent").is_none());
    assert!(f.store.exists(&staging).await.unwrap());
    f.store.delete(&staging).await.unwrap();
    assert_eq!(
        f.cleanup().await,
        0,
        "staging disappearance must not cause retries"
    );
    assert_eq!(f.state(id).await.0, "RETIRED");
}
