use serde_json::{Value, json};
use std::time::Duration;
use workflow_expression::{
    Bindings, Category, Engine, Position, StepField, StepRequest, WorkerConfig, WorkerError,
};

fn field(path: &str, position: Position, template: Value) -> StepField {
    StepField {
        path: path.into(),
        position,
        template,
        value_from: None,
    }
}

#[tokio::test]
async fn batch_positions_snapshot_numeric_ordering_and_sanitization() {
    let engine = Engine::new(WorkerConfig {
        workers: 1,
        cache_entries: 32,
        cache_bytes: 1024 * 1024,
        ..WorkerConfig::default()
    })
    .unwrap();
    assert_eq!(engine.config().workers, 1);
    assert_eq!(engine.config().stack_bytes, 8 * 1024 * 1024);
    assert_eq!(engine.config().stack_reservation(), Some(8 * 1024 * 1024));
    let context = json!({"old":3,"workflow":"shadow","secret":"W4_SENTINEL_SECRET","decimal":1.25,"integer":u64::MAX});
    let input = json!({"old":7});
    let bindings =
        || Bindings::new(&context, &input, Some(&json!({"old":11})), Some(&json!(3))).unwrap();
    let request = |fields, first_true| StepRequest {
        bindings: bindings(),
        fields,
        first_true,
    };
    let fields = vec![
        field(
            "/set",
            Position::Set,
            json!({"literal":"hello","embedded":"v=${string(context.old)}","whole":"${workflow.input.old}","decimal":"${context.decimal}","integer":"${context.integer}"}),
        ),
        field(
            "/export/as/a",
            Position::Export,
            json!("${context.old + output.old}"),
        ),
        field(
            "/export/as/b",
            Position::RunnerExport,
            json!("${context.old}"),
        ),
        field(
            "/assert/json/x",
            Position::AssertJsonPredicate,
            json!("${value == context.old}"),
        ),
        field(
            "/output/as",
            Position::WorkflowOutput,
            json!({"decimal":"${context.decimal}","integer":"${context.integer}"}),
        ),
    ];
    let values = engine.step(request(fields, false)).await.unwrap();
    assert_eq!(
        values[0],
        json!({"literal":"hello","embedded":"v=3","whole":7,"decimal":1.25,"integer":u64::MAX})
    );
    assert_eq!(&values[1..4], &[json!(14), json!(3), json!(true)]);
    assert_eq!(values[4], json!({"decimal":1.25,"integer":u64::MAX}));
    assert_eq!(context["old"], json!(3));
    // The same cached source must be audited again for the receiving Position.
    let forbidden = engine
        .step(request(
            vec![field("/set", Position::Set, json!("${output.old}"))],
            false,
        ))
        .await
        .unwrap_err();
    assert!(matches!(forbidden.error, WorkerError::Expression(_)));
    assert!(!forbidden.to_string().contains("W4_SENTINEL_SECRET"));
    let short = engine
        .step(request(
            vec![
                field("/switch/0", Position::SwitchWhen, json!("${true}")),
                field("/switch/1", Position::SwitchWhen, json!("${missing}")),
            ],
            true,
        ))
        .await
        .unwrap();
    assert_eq!(short, vec![json!(true)]);
    let error = engine
        .step(request(
            vec![field("/switch/0", Position::SwitchWhen, json!("${1}"))],
            true,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(error.error,WorkerError::Expression(ref e) if e.category==Category::ResultType)
    );
    let error = engine
        .step(request(
            vec![
                field("/export/as/a", Position::Export, json!("${missing}")),
                field("/export/as/b", Position::Export, json!("${otherMissing}")),
            ],
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.path, "/export/as/a");
    assert!(!error.to_string().contains("missing"));
    let error = engine
        .step(request(
            vec![field("/output/as", Position::WorkflowOutput, json!("${1}"))],
            false,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(error.error,WorkerError::Expression(ref e) if e.category==Category::ResultType)
    );
    tokio::task::spawn_blocking(move || engine.shutdown(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn aggregate_batch_limits_cached_binding_rules_and_nested_paths() {
    use workflow_expression::Limits;
    let engine = Engine::new(WorkerConfig {
        workers: 1,
        cache_entries: 16,
        cache_bytes: 64 * 1024,
        ..WorkerConfig::default()
    })
    .unwrap();
    let context = json!({"large":"x".repeat(50),"actual":{"n":3}});
    let bindings = Bindings::with_limits(
        &context,
        &json!({}),
        None,
        None,
        Limits {
            output_bytes: 80,
            ..Limits::default()
        },
    )
    .unwrap();
    let error = engine
        .step(StepRequest {
            bindings,
            first_true: false,
            fields: vec![
                field("/a", Position::Set, json!("${context.large}")),
                field("/b", Position::Set, json!("${context.large}")),
            ],
        })
        .await
        .unwrap_err();
    assert_eq!(error.path, "/b");
    assert!(matches!(error.error,WorkerError::Expression(ref e) if e.category==Category::Limit));
    let exact = Bindings::with_limits(
        &json!({"exact":"x".repeat(78)}),
        &json!({}),
        None,
        None,
        Limits {
            output_bytes: 80,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(
        engine
            .step(StepRequest {
                bindings: exact,
                first_true: false,
                fields: vec![field("/exact", Position::Set, json!("${context.exact}"))]
            })
            .await
            .unwrap(),
        vec![json!("x".repeat(78))]
    );
    let mut predicate = field(
        "/assert/json/n",
        Position::AssertJsonPredicate,
        json!("${value == 3}"),
    );
    predicate.value_from = Some((0, "$.n".into()));
    let error = engine
        .step(StepRequest {
            bindings: Bindings::new(&context, &json!({}), None, None).unwrap(),
            first_true: false,
            fields: vec![
                field(
                    "/assert/value",
                    Position::AssertValue,
                    json!("${context.actual}"),
                ),
                predicate,
                field(
                    "/assert/json/z/equals",
                    Position::AssertJsonEquals,
                    json!("${value == 3}"),
                ),
            ],
        })
        .await
        .unwrap_err();
    assert_eq!(error.path, "/assert/json/z/equals");
    assert!(matches!(error.error,WorkerError::Expression(ref e) if e.category==Category::Invalid));
    let error = engine
        .step(StepRequest {
            bindings: Bindings::new(&json!({}), &json!({}), None, None).unwrap(),
            first_true: false,
            fields: vec![field(
                "/set",
                Position::Set,
                json!({"a/b":[{"~key":"a ${missing} b"}],"z":"${other}"}),
            )],
        })
        .await
        .unwrap_err();
    assert_eq!(error.path, "/set/a~1b/0/~0key");
    assert!(
        matches!(error.error,WorkerError::Expression(ref e) if e.span_index==Some(0) && e.offset.is_some())
    );
    tokio::task::spawn_blocking(move || engine.shutdown(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
}
