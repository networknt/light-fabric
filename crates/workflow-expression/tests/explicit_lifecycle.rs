use serde_json::json;
use std::time::Duration;
use workflow_expression::*;

#[test]
fn initialization_and_access_fail_without_panics_or_expression_reclassification() {
    let mut cache = CompileCache::new().unwrap();
    assert!(matches!(CompileCache::new(), Err(WorkerError::Full)));
    assert!(matches!(
        CompileCache::with_budget(129, 1),
        Err(WorkerError::Configuration)
    ));
    assert_eq!(cache.len(), Ok(0));
    let handle = cache
        .compile(Profile::CelWorkflowV2, "1", Position::Set)
        .unwrap();
    cache.shutdown(Duration::from_secs(2)).unwrap();
    assert_eq!(cache.len(), Err(WorkerError::Unavailable));
    assert_eq!(cache.is_empty(), Err(WorkerError::Unavailable));
    assert_eq!(cache.retained_bytes(), Err(WorkerError::Unavailable));
    assert!(matches!(
        cache.compile(Profile::CelWorkflowV2, "1", Position::Set),
        Err(WorkerError::Unavailable)
    ));
    assert!(matches!(
        cache.template("${ 1 }", Position::Set),
        Err(WorkerError::Unavailable)
    ));
    assert!(matches!(
        cache.json_template(&json!("${ 1 }"), Position::Set),
        Err(WorkerError::Unavailable)
    ));
    let bindings = Bindings::new(&json!({}), &json!(null), None, None).unwrap();
    assert_eq!(
        evaluate(&handle, &bindings, Kind::Json),
        Err(WorkerError::Unavailable)
    );
}

#[test]
fn uncached_owner_has_explicit_shutdown_and_dropped_owner_fences_handles() {
    let engine = Engine::new(WorkerConfig {
        workers: 1,
        cache_entries: 0,
        cache_bytes: 0,
        ..WorkerConfig::default()
    })
    .unwrap();
    let handle = compile(&engine, "1", Position::Set).unwrap();
    let bindings = Bindings::new(&json!({}), &json!(null), None, None).unwrap();
    assert_eq!(evaluate(&handle, &bindings, Kind::Json), Ok(json!(1)));
    engine.shutdown(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        compile(&engine, "1", Position::Set),
        Err(WorkerError::Unavailable)
    ));
    drop(engine);
    assert_eq!(
        evaluate(&handle, &bindings, Kind::Json),
        Err(WorkerError::InvalidHandle)
    );
}

#[tokio::test]
async fn synchronous_access_in_async_context_is_fallible_and_async_access_works() {
    let engine = Engine::new(WorkerConfig {
        workers: 1,
        cache_entries: 0,
        cache_bytes: 0,
        ..WorkerConfig::default()
    })
    .unwrap();
    assert!(matches!(
        compile(&engine, "1", Position::Set),
        Err(WorkerError::BlockingInAsync)
    ));
    assert_eq!(engine.cache_stats(), Err(WorkerError::BlockingInAsync));
    let handle = engine
        .enqueue(
            Profile::CelWorkflowV2,
            "1".into(),
            Position::Set,
            Limits::default(),
        )
        .unwrap()
        .receive()
        .await
        .unwrap();
    drop(handle);
    engine.shutdown(Duration::from_secs(2)).unwrap();
}
