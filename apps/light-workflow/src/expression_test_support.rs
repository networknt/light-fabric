//! Coordination for fixtures sharing the evaluator's process-wide reservation.
//! Independent Engines are retained; only resource-owning fixtures wait here.
use std::sync::Mutex;
use workflow_expression::{Engine, WorkerConfig, WorkerError};

static PERMIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static ENGINES: Mutex<Vec<Engine>> = Mutex::new(Vec::new());

pub(crate) struct FixturePermit {
    _permit: tokio::sync::MutexGuard<'static, ()>,
}

pub(crate) async fn acquire() -> FixturePermit {
    FixturePermit {
        _permit: PERMIT.lock().await,
    }
}

pub(crate) fn engine(config: WorkerConfig) -> Result<Engine, WorkerError> {
    let engine = Engine::new(config)?;
    ENGINES.lock().unwrap().push(engine.clone());
    Ok(engine)
}

impl Drop for FixturePermit {
    fn drop(&mut self) {
        // Also runs during assertion unwinding. Shutdown joins workers and
        // releases their cache reservation before the permit field is dropped.
        for engine in ENGINES.lock().unwrap().drain(..) {
            let result = engine.shutdown(std::time::Duration::from_secs(2));
            if !std::thread::panicking() {
                result.expect("fixture workers must shut down before releasing coordination");
            }
        }
    }
}
