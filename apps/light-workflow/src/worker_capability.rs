//! Capability evidence only: no policy writes, admission permits or rollout decisions.
use crate::profile_support::SupportedProfiles;
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
static PROCESS_INSTANCE_ID: OnceLock<Uuid> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub instance_id: Uuid,
    pub binary_version: String,
    pub supported: SupportedProfiles,
    pub admitting: SupportedProfiles,
}
impl Capability {
    pub fn new(supported: SupportedProfiles, validator_available: bool) -> Self {
        Self {
            instance_id: *PROCESS_INSTANCE_ID.get_or_init(Uuid::new_v4),
            binary_version: env!("CARGO_PKG_VERSION").into(),
            admitting: SupportedProfiles::from_evaluator(
                validator_available && supported.profiles().contains(&"cel-workflow-v2"),
            ),
            supported,
        }
    }
}

#[async_trait::async_trait]
pub trait CapabilityStore: Send + Sync {
    async fn write(&self, capability: &Capability) -> Result<(), sqlx::Error>;
}
#[async_trait::async_trait]
impl CapabilityStore for sqlx::PgPool {
    async fn write(&self, c: &Capability) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO workflow_ops.workflow_worker_capability_t(instance_id,binary_version,supported_profiles,admits_profiles,heartbeat_ts) VALUES($1,$2,$3,$4,clock_timestamp()) ON CONFLICT(instance_id) DO UPDATE SET binary_version=EXCLUDED.binary_version,supported_profiles=EXCLUDED.supported_profiles,admits_profiles=EXCLUDED.admits_profiles,heartbeat_ts=EXCLUDED.heartbeat_ts")
            .bind(c.instance_id).bind(&c.binary_version).bind(c.supported.profiles()).bind(c.admitting.profiles()).execute(self).await?;
        Ok(())
    }
}
pub struct Heartbeat {
    capability: Capability,
    store: Arc<dyn CapabilityStore>,
}
impl Heartbeat {
    pub async fn start(
        capability: Capability,
        store: Arc<dyn CapabilityStore>,
    ) -> Result<Self, sqlx::Error> {
        store.write(&capability).await?;
        Ok(Self { capability, store })
    }
    pub fn capability(&self) -> &Capability {
        &self.capability
    }
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), sqlx::Error> {
        self.run_ticks(shutdown, || tokio::time::sleep(HEARTBEAT_INTERVAL))
            .await
    }
    async fn run_ticks<F, T>(
        self,
        shutdown: CancellationToken,
        mut tick: F,
    ) -> Result<(), sqlx::Error>
    where
        F: FnMut() -> T,
        T: std::future::Future<Output = ()>,
    {
        loop {
            tokio::select! { biased;
                _ = shutdown.cancelled() => return Ok(()),
                _ = tick() => {},
            }
            tokio::select! { biased;
                _ = shutdown.cancelled() => return Ok(()),
                result = self.store.write(&self.capability) => result?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Store {
        writes: Mutex<Vec<Capability>>,
        fail_at: usize,
    }
    #[async_trait::async_trait]
    impl CapabilityStore for Store {
        async fn write(&self, c: &Capability) -> Result<(), sqlx::Error> {
            let mut writes = self.writes.lock().unwrap();
            if writes.len() == self.fail_at {
                return Err(sqlx::Error::Protocol("fixture write failure".into()));
            }
            writes.push(c.clone());
            Ok(())
        }
    }
    #[tokio::test]
    async fn startup_failure_never_returns_a_running_heartbeat() {
        assert!(
            Heartbeat::start(
                Capability::new(SupportedProfiles::from_evaluator(true), true),
                Arc::new(Store::default())
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn identity_capabilities_refresh_failure_and_shutdown() {
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(30));
        let store = Arc::new(Store {
            fail_at: 2,
            ..Default::default()
        });
        let c = Capability::new(SupportedProfiles::from_evaluator(true), true);
        let h = Heartbeat::start(c.clone(), store.clone()).await.unwrap();
        assert!(
            h.run_ticks(CancellationToken::new(), || std::future::ready(()))
                .await
                .is_err()
        );
        assert_eq!(*store.writes.lock().unwrap(), vec![c.clone(), c]);
        let no_v2 = Capability::new(SupportedProfiles::from_evaluator(false), true);
        assert_eq!(
            store.writes.lock().unwrap()[0].instance_id,
            no_v2.instance_id
        );
        assert_eq!(no_v2.admitting.profiles(), vec!["cel-workflow-v1"]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let h = Heartbeat::start(
            no_v2,
            Arc::new(Store {
                fail_at: usize::MAX,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        h.run_ticks(cancel, std::future::pending).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_an_inflight_refresh_without_another_write() {
        struct BlockingStore {
            writes: std::sync::atomic::AtomicUsize,
            refreshing: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl CapabilityStore for BlockingStore {
            async fn write(&self, _: &Capability) -> Result<(), sqlx::Error> {
                if self
                    .writes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    > 0
                {
                    self.refreshing.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(())
            }
        }
        let store = Arc::new(BlockingStore {
            writes: std::sync::atomic::AtomicUsize::new(0),
            refreshing: tokio::sync::Notify::new(),
        });
        let heartbeat = Heartbeat::start(
            Capability::new(SupportedProfiles::from_evaluator(true), false),
            store.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            heartbeat.capability().binary_version,
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            heartbeat.capability().admitting.profiles(),
            vec!["cel-workflow-v1"]
        );
        let cancel = CancellationToken::new();
        let running = tokio::spawn(heartbeat.run_ticks(cancel.clone(), || std::future::ready(())));
        tokio::time::timeout(Duration::from_secs(1), store.refreshing.notified())
            .await
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refresh_failure_uses_managed_health_and_cancels_siblings() {
        let root = CancellationToken::new();
        let admission = light_runtime::AdmissionGate::default();
        admission.open();
        let health = crate::rule_api::WorkflowHealth::default();
        let heartbeat = Heartbeat::start(
            Capability::new(SupportedProfiles::from_evaluator(true), true),
            Arc::new(Store {
                fail_at: 1,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let task = crate::service_runtime::ManagedWorkflowTask::spawn(
            "w6-heartbeat-fixture",
            root.clone(),
            admission.clone(),
            health.clone(),
            None,
            move |cancel| heartbeat.run_ticks(cancel, || std::future::ready(())),
        );
        tokio::time::timeout(Duration::from_secs(1), root.cancelled())
            .await
            .unwrap();
        assert!(!health.is_ready());
        assert!(admission.has_failed());
        assert!(!admission.try_open());
        drop(task);
    }
}
