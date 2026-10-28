pub mod support;
use gateway_policy::observation::{RecordKind, RecordSink};
use serverless_core::{handlers::metrics::MetricsHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;
#[derive(Default)]
struct Records(Mutex<Vec<serde_json::Value>>);
impl RecordSink for Records {
    fn try_emit(&self, kind: RecordKind, value: serde_json::Value) {
        assert_eq!(kind, RecordKind::Emf);
        self.0.lock().unwrap().push(value);
    }
}

fn route_map(template: &str, method: &str) -> TargetRoutes {
    TargetRoutes::compile([(
        format!("{template}@{method}"),
        Target::Function("FnTest".into()),
    )])
    .unwrap()
}
fn settings() -> serde_json::Value {
    config(
        &[(HandlerId::Metrics, "metrics"), (HandlerId::Proxy, "proxy")],
        &["metrics", "proxy"],
    )
}
fn metrics_registry(
    fallback: TargetRoutes,
    dispatch: TargetRoutes,
    records: Arc<Records>,
) -> Registry {
    let mut registry = registry();
    install(&mut registry, HandlerId::Proxy, Binding::Dispatch(dispatch));
    registry.register(
        HandlerId::Metrics,
        Arc::new(
            MetricsHandler::new("test-only-gateway".into(), records)
                .unwrap()
                .with_routes(Arc::new(fallback)),
        ),
    );
    registry
}
fn rejected(value: serde_json::Value, registry: &Registry) -> bool {
    match ExecutionPlan::compile(value, serde_json::json!({}), registry) {
        Err(error) => {
            assert_eq!(
                error,
                InitError::Malformed("metrics trusted route template byte limit (1024)")
            );
            assert_eq!(
                error.to_string(),
                "InvalidConfiguration: Malformed(\"metrics trusted route template byte limit (1024)\")"
            );
            true
        }
        Ok(_) => false,
    }
}
fn templates(bytes: usize) -> Vec<String> {
    vec![
        format!("/{}", "a".repeat(bytes - 1)),
        format!("/{{{}}}", "a".repeat(bytes - 3)),
        format!("/{}/*", "a".repeat(bytes - 3)),
    ]
}

#[test]
fn utf8_byte_boundary_literal_parameter_wildcard_and_path_sources() {
    for bytes in [1024, 1025] {
        for template in templates(bytes).into_iter().chain(std::iter::once(format!(
            "/{}{}",
            "é".repeat(511),
            "a".repeat(bytes - 1023)
        ))) {
            assert_eq!(template.len(), bytes);
            let registry = metrics_registry(
                route_map(&template, "get"),
                route_map(&template, "get"),
                Arc::new(Records::default()),
            );
            assert_eq!(rejected(settings(), &registry), bytes > 1024);
            // Path-selected Metrics with an independent, short dispatch map.
            let registry = metrics_registry(
                TargetRoutes::default(),
                targets(),
                Arc::new(Records::default()),
            );
            let mut value = settings();
            value.as_object_mut().unwrap().remove("defaultHandlers");
            value["paths"] =
                serde_json::json!([{"path":template,"method":"get","exec":["metrics","proxy"]}]);
            assert_eq!(rejected(value, &registry), bytes > 1024);
        }
    }
}

#[test]
fn independent_with_routes_fallback_cannot_bypass_admission() {
    let template = templates(1025).remove(0);
    let records = Arc::new(Records::default());
    // The dispatch catalogue is short and unrelated: only the borrowed Metrics
    // fallback catalogue reveals this previously unprotected early-exit input.
    let registry = metrics_registry(
        route_map(&template, "get"),
        route_map("/short", "get"),
        records.clone(),
    );
    assert!(rejected(settings(), &registry));
    assert!(records.0.lock().unwrap().is_empty());
}

#[test]
fn disabled_unselected_nested_unreachable_and_unrelated_templates_are_excluded() {
    let template = templates(1025).remove(0);
    let mut registry = metrics_registry(
        route_map(&template, "get"),
        route_map(&template, "get"),
        Arc::new(Records::default()),
    );
    let mut value = settings();
    value["defaultHandlers"] = serde_json::json!(["proxy"]);
    value["chains"] = serde_json::json!({"unused":["nested"],"nested":["metrics","proxy"]});
    assert!(!rejected(value.clone(), &registry));
    registry.disable(HandlerId::Metrics);
    assert!(!rejected(settings(), &registry));

    let registry = metrics_registry(
        route_map(&template, "get"),
        route_map(&template, "get"),
        Arc::new(Records::default()),
    );
    value["paths"] =
        serde_json::json!([{"path":"/selected/*","method":"get","exec":["metrics","proxy"]}]);
    assert!(!rejected(value.clone(), &registry)); // Disjoint path language.
    value["paths"] =
        serde_json::json!([{"path":"/{id}","method":"post","exec":["metrics","proxy"]}]);
    assert!(!rejected(value, &registry)); // Method mismatch.

    let mut value = settings();
    value["paths"] = serde_json::json!([{"path":"/{id}","method":"get","exec":["proxy"]}]);
    assert!(!rejected(value, &registry)); // Default cannot select this literal.
    // A matching selectable chain makes the same metadata subject to admission.
    let mut value = settings();
    value["defaultHandlers"] = serde_json::json!(["proxy"]);
    value["paths"] =
        serde_json::json!([{"path":"/{id}","method":"get","exec":["metrics","proxy"]}]);
    assert!(rejected(value, &registry));
}

#[test]
fn proxy_precedence_can_make_an_oversized_parameter_template_unreachable() {
    let oversized = templates(1025).remove(1);
    // A literal is a different reachable language: it must not
    // mask the oversized parameter route for other one-segment paths.
    let routes = TargetRoutes::compile([
        ("/fixed@get".into(), Target::Function("FnTest".into())),
        (
            format!("{oversized}@get"),
            Target::Function("FnTest".into()),
        ),
    ])
    .unwrap();
    let registry = metrics_registry(routes.clone(), routes, Arc::new(Records::default()));
    assert!(rejected(settings(), &registry));
    // A non-Metrics path with the same full parameter language excludes the
    // oversized fallback for the default Metrics chain, regardless of names.
    let mut value = settings();
    value["paths"] = serde_json::json!([{"path":"/{safeName}","method":"get","exec":["proxy"]}]);
    assert!(!rejected(value, &registry));
}

#[test]
fn nested_selected_occurrences_and_default_noop_other_handlers() {
    let template = templates(1025).remove(0);
    let registry = metrics_registry(
        route_map(&template, "get"),
        targets(),
        Arc::new(Records::default()),
    );
    let mut value = settings();
    value["chains"] = serde_json::json!({"outer":["inner"],"inner":["metrics","metrics","proxy"]});
    value["defaultHandlers"] = serde_json::json!(["outer"]);
    assert!(rejected(value, &registry));
    let counts = Arc::new(Counts::default());
    let handler = Handler::new(HandlerId::Correlation, Action::Continue, &counts);
    assert!(handler.metrics_fallback_routes().is_none());
    assert_eq!(n(&counts.request_entries), 0);
    assert_eq!(n(&counts.backend), 0);
}

#[tokio::test]
async fn accepted_boundary_emits_exact_route_for_normal_and_initial_early_exits() {
    let template = templates(1024).remove(1);
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let mut registry = metrics_registry(
        route_map(&template, "get"),
        route_map(&template, "get"),
        records.clone(),
    );
    install(
        &mut registry,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    let mut value = settings();
    value["handlers"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::Value::String(entry(
            HandlerId::ResponseHeader,
            "response",
        )));
    value["defaultHandlers"] = serde_json::json!(["metrics", "proxy", "response"]);
    let pipeline = Pipeline::new(compile(value, &registry), vec![counts.clone()]);
    for (index, outcome) in ["normal", "deadline", "cancel"].into_iter().enumerate() {
        let mut inv = invocation();
        inv.path = "/synthetic-private-id".into();
        if outcome == "deadline" {
            inv.deadline = tokio::time::Instant::now();
        }
        if outcome == "cancel" {
            inv.cancel.cancel();
        }
        let result = pipeline.execute(inv, &FakeBackend::new(&counts)).await;
        assert_eq!(
            result.status.as_u16(),
            if outcome == "normal" { 200 } else { 504 }
        );
        let data = records.0.lock().unwrap();
        assert_eq!(data.last().unwrap()["Route"], template);
        assert_eq!(data.last().unwrap()["Requests"], 1);
        assert_eq!(
            data.last().unwrap()["Errors"],
            u8::from(outcome != "normal")
        );
        assert!(
            !data
                .last()
                .unwrap()
                .to_string()
                .contains("synthetic-private-id")
        );
        assert_eq!(n(&counts.backend), 1);
        assert_eq!(n(&counts.response_entries), index + 1);
        assert_eq!(n(&counts.finalized), index + 1);
        assert_eq!(data.len(), index + 1);
    }
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.response_entries), 3);
    assert_eq!(n(&counts.finalized), 3);
    assert_eq!(records.0.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn matched_trusted_template_must_not_turn_into_an_unapproved_truncation_marker() {
    let template = format!("/configured/{{{}}}", "a".repeat(1024));
    assert!(template.len() > 1024);
    let routes =
        TargetRoutes::compile([(format!("{template}@get"), Target::Function("FnTest".into()))])
            .unwrap();
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    install(&mut r, HandlerId::Proxy, Binding::Dispatch(routes.clone()));
    r.register(
        HandlerId::Metrics,
        Arc::new(
            MetricsHandler::new("test-only-gateway".into(), records.clone())
                .unwrap()
                .with_routes(Arc::new(routes)),
        ),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    let result = ExecutionPlan::compile(
        config(
            &[
                (HandlerId::Metrics, "metrics"),
                (HandlerId::Proxy, "proxy"),
                (HandlerId::ResponseHeader, "response"),
            ],
            &["metrics", "proxy", "response"],
        ),
        serde_json::json!({}),
        &r,
    );
    assert!(matches!(
        result,
        Err(InitError::Malformed(
            "metrics trusted route template byte limit (1024)"
        ))
    ));
    assert_eq!(n(&counts.request_entries), 0);
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.response_entries), 0);
    assert_eq!(n(&counts.finalized), 0);
    assert!(records.0.lock().unwrap().is_empty());
}
