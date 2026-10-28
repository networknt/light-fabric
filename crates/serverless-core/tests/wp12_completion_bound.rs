pub mod support;
use gateway_policy::observation::{RecordKind, RecordSink};
use serde_json::Value;
use serverless_core::{handlers::metrics::MetricsHandler, plan::*, *};
use std::sync::Arc;
use support::*;
struct NoEmission;
impl RecordSink for NoEmission {
    fn try_emit(&self, _: RecordKind, _: Value) {
        panic!("compilation must not emit or invoke policies");
    }
}
#[derive(Default)]
struct Captured(std::sync::Mutex<Vec<Value>>);
impl RecordSink for Captured {
    fn try_emit(&self, _: RecordKind, value: Value) {
        self.0.lock().unwrap().push(value);
    }
}
#[tokio::test]
async fn sixty_four_descriptors_prepare_and_drain_once_on_success_deadline_and_cancellation() {
    for mode in 0..3 {
        let records = Arc::new(Captured::default());
        let counts = Arc::new(Counts::default());
        let mut r = registry();
        r.register(
            HandlerId::Metrics,
            Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
        );
        let mut sequence = vec!["metrics"; 64];
        sequence.push("proxy");
        let p = Pipeline::new(
            compile(
                config(
                    &[(HandlerId::Metrics, "metrics"), (HandlerId::Proxy, "proxy")],
                    &sequence,
                ),
                &r,
            ),
            vec![counts.clone()],
        );
        let mut inv = invocation();
        if mode == 1 {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        if mode == 2 {
            inv.cancel.cancel();
        }
        let started = std::time::Instant::now();
        let response = p.execute(inv, &FakeBackend::new(&counts)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "local preparation/drain qualification budget"
        );
        assert_eq!(n(&counts.backend), usize::from(mode == 0));
        assert_eq!(n(&counts.finalized), 1);
        let data = records.0.lock().unwrap();
        assert_eq!(data.len(), 64);
        assert!(
            data.iter()
                .all(|v| v["Requests"] == 1 && v["Errors"] == u64::from(mode != 0))
        );
        assert_eq!(response.status.is_success(), mode == 0);
    }
}
fn observation_registry() -> Registry {
    let mut r = registry();
    r.register(
        HandlerId::Metrics,
        Arc::new(MetricsHandler::new("test-only-gateway".into(), Arc::new(NoEmission)).unwrap()),
    );
    r.register(HandlerId::Audit, Arc::new(serverless_core::handlers::audit::AuditHandler::from_value(
        &serde_json::json!({"enabled":true,"audit":["serviceId"],"timestampFormat":"'test'"}),
        "test-only-gateway".into(), Arc::new(NoEmission)).unwrap()));
    r
}
#[test]
fn zero_one_and_sixty_four_mixed_occurrences_are_accepted() {
    let r = observation_registry();
    for count in [0, 1, 64, 65] {
        let mut sequence = (0..count)
            .map(|i| if i % 2 == 0 { "audit" } else { "metrics" })
            .collect::<Vec<_>>();
        sequence.push("proxy");
        let result = ExecutionPlan::compile(
            config(
                &[
                    (HandlerId::Audit, "audit"),
                    (HandlerId::Metrics, "metrics"),
                    (HandlerId::Proxy, "proxy"),
                ],
                &sequence,
            ),
            serde_json::json!({}),
            &r,
        );
        if count <= 64 {
            assert_eq!(
                result.unwrap().handler_ids(&invocation()).unwrap().len(),
                count + 1
            );
        } else {
            assert!(matches!(
                result,
                Err(InitError::Malformed(
                    "active audit/metrics occurrence limit (64)"
                ))
            ));
        }
    }
}
#[test]
fn nested_repetitions_count_but_unreachable_definitions_do_not() {
    let r = observation_registry();
    let mut value = config(
        &[
            (HandlerId::Audit, "audit"),
            (HandlerId::Metrics, "metrics"),
            (HandlerId::Proxy, "proxy"),
        ],
        &["outer", "proxy"],
    );
    value["chains"] = serde_json::json!({"pair":["audit","metrics"],"outer":vec!["pair";32],"unused":vec!["metrics";65]});
    ExecutionPlan::compile(value.clone(), serde_json::json!({}), &r).unwrap();
    value["chains"]["outer"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("audit"));
    assert!(matches!(
        ExecutionPlan::compile(value, serde_json::json!({}), &r),
        Err(InitError::Malformed(
            "active audit/metrics occurrence limit (64)"
        ))
    ));
}
#[test]
fn path_chains_are_bounded_and_disabled_occurrences_are_excluded() {
    let mut r = observation_registry();
    let mut value = config(
        &[(HandlerId::Metrics, "metrics"), (HandlerId::Proxy, "proxy")],
        &["proxy"],
    );
    let mut sequence = vec!["metrics"; 65];
    sequence.push("proxy");
    value["paths"] = serde_json::json!([{"path":"/x","method":"GET","exec":sequence}]);
    assert!(ExecutionPlan::compile(value.clone(), serde_json::json!({}), &r).is_err());
    r.disable(HandlerId::Metrics);
    assert_eq!(
        ExecutionPlan::compile(value, serde_json::json!({}), &r)
            .unwrap()
            .handler_ids(&invocation())
            .unwrap(),
        vec![HandlerId::Proxy]
    );
}
#[test]
fn selected_observation_descriptor_count_requires_an_init_bound() {
    let mut r = registry();
    r.register(
        HandlerId::Metrics,
        Arc::new(MetricsHandler::new("test-only-gateway".into(), Arc::new(NoEmission)).unwrap()),
    );
    let ids = [(HandlerId::Metrics, "metrics"), (HandlerId::Proxy, "proxy")];
    let mut sequence = vec!["metrics"; 65];
    sequence.push("proxy");
    let result = ExecutionPlan::compile(config(&ids, &sequence), serde_json::json!({}), &r);
    if let Ok(plan) = &result {
        assert_eq!(
            plan.handler_ids(&invocation())
                .unwrap()
                .iter()
                .filter(|id| **id == HandlerId::Metrics)
                .count(),
            65
        );
    }
    assert!(
        result.is_err(),
        "proposed total preparation bound64: real compiler currently admits65 selected observation occurrences; no backend or policy was executed"
    );
}

#[test]
fn sixty_five_unselected_catalogue_entries_and_unrelated_policies_are_not_capped() {
    let mut r = observation_registry();
    let mut value = config(&[(HandlerId::Proxy, "proxy")], &["proxy"]);
    for i in 0..65 {
        value["handlers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(entry(
                HandlerId::Metrics,
                &format!("unused{i}")
            )));
    }
    assert_eq!(
        ExecutionPlan::compile(value, serde_json::json!({}), &r)
            .unwrap()
            .handler_ids(&invocation())
            .unwrap(),
        vec![HandlerId::Proxy]
    );
    let counts = Arc::new(Counts::default());
    install(
        &mut r,
        HandlerId::RequestHeader,
        Binding::Request(
            Arc::new(Handler::new(
                HandlerId::RequestHeader,
                Action::Continue,
                &counts,
            )),
            RequestCapabilities::default(),
        ),
    );
    let mut sequence = vec!["header"; 65];
    sequence.push("proxy");
    let plan = ExecutionPlan::compile(
        config(
            &[
                (HandlerId::RequestHeader, "header"),
                (HandlerId::Proxy, "proxy"),
            ],
            &sequence,
        ),
        serde_json::json!({}),
        &r,
    )
    .unwrap();
    assert_eq!(plan.handler_ids(&invocation()).unwrap().len(), 66);
    assert_eq!(n(&counts.request_entries), 0);
}
