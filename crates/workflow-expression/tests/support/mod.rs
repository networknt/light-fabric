use workflow_expression::{Engine, ExpressionError, WorkerConfig, WorkerError};
pub fn owner() -> Engine {
    Engine::new(WorkerConfig {
        workers: 1,
        cache_entries: 0,
        cache_bytes: 0,
        ..WorkerConfig::default()
    })
    .unwrap()
}
pub fn diagnostic(error: impl Into<WorkerError>) -> ExpressionError {
    match error.into() {
        WorkerError::Expression(error) => error,
        error => panic!("unexpected worker failure in expression-contract assertion: {error}"),
    }
}
