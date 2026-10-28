//! Pure isolated admission failures. No image open, memfd, command or worker.
use super::*;
use crate::validator_ipc::{
    ownership::Drain,
    supervision::{Authority, Phase, Policy},
};
use std::io::Write;
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
fn exported(d: &InitDiagnostic) -> serde_json::Value {
    let output = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(output.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || d.emit_after_run());
    let bytes = output.0.lock().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    rows[0]["fields"].clone()
}
fn policy() -> Policy {
    Policy {
        platform_bytes: 1048576,
        parent_requested_bytes: 262144,
        worker_requested_bytes: 262144,
        headroom_bytes: 524288,
        worker_address_space_bytes: 1048576,
        lifetime_cpu_seconds: 1,
    }
}
async fn prepare(
    authority: Arc<Authority>,
    drain: Arc<Drain>,
    diagnostic: Arc<InitDiagnostic>,
) -> Result<(Controller, crate::validator_ipc::ownership::Initialization), Failure> {
    Controller::prepare_supervised_async_observed(
        &crate::bundle_snapshot::Pin {
            path: "/never-opened/admission-worker".into(),
            bytes: 64,
            sha256: [1; 32],
        },
        Arc::from(&b"x"[..]),
        SourceFormat::Yaml,
        Arc::new(crate::validator_ipc::Settings::default()),
        policy(),
        Instant::now() + Duration::from_secs(1),
        (authority, drain, None, Some(diagnostic)),
    )
    .await
}
#[tokio::test]
async fn closed_drain_is_reported_before_image_or_owner_admission() {
    let authority = Authority::isolated();
    let drain = Drain::new();
    drain.close();
    let d = Arc::new(InitDiagnostic::default());
    assert!(matches!(
        prepare(authority.clone(), drain.clone(), d.clone()).await,
        Err(Failure::NoReady)
    ));
    assert_eq!(authority.phase(), Phase::Idle);
    assert_eq!(drain.snapshot().captures, 0);
    assert_eq!(
        exported(&d),
        serde_json::json!({"stage":"controller_drain","failure":"no_ready"})
    );
}
#[tokio::test]
async fn busy_owner_is_reported_without_disturbing_the_incumbent_reservation() {
    let authority = Authority::isolated();
    let owner = authority.reserve_owner().unwrap();
    let drain = Drain::new();
    let d = Arc::new(InitDiagnostic::default());
    assert!(matches!(
        prepare(authority.clone(), drain.clone(), d.clone()).await,
        Err(Failure::NoReady)
    ));
    assert_eq!(authority.phase(), Phase::Reserved);
    assert_eq!(drain.snapshot().captures, 0);
    assert!(!drain.is_closed());
    assert_eq!(
        exported(&d),
        serde_json::json!({"stage":"controller_owner_reserve","failure":"no_ready_busy"})
    );
    drop(owner);
    assert_eq!(authority.phase(), Phase::Idle);
}
