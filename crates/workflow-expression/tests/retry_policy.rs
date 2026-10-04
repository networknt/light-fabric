use serde_json::{Value, json};
use workflow_expression::*;
fn definition(policy: Value) -> Value {
    json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"fetch":{"call":"http","with":{},"retry":policy}}]})
}
fn supported() -> Value {
    json!({"limit":{"attempt":{"count":3}},"delay":{"seconds":2}})
}
#[test]
fn fixed_count_delay_defaults_and_reference_parity() {
    let mut raw = definition(supported());
    raw["use"] = json!({"retries":{"fixed":supported()}});
    assert_eq!(
        resolve_retry_policy(&raw, &supported(), "/retry", None).unwrap(),
        FixedRetry {
            attempts: 3,
            delay_ms: 2000
        }
    );
    assert_eq!(
        resolve_retry_policy(&raw, &json!("fixed"), "/retry", None).unwrap(),
        FixedRetry {
            attempts: 3,
            delay_ms: 2000
        }
    );
    assert_eq!(
        resolve_retry_policy(&raw, &json!({}), "/retry", None).unwrap(),
        FixedRetry {
            attempts: 1,
            delay_ms: 0
        }
    );
    raw["do"][0]["fetch"]["retry"] = json!("fixed");
    assert!(validate_retry_policies(&raw).is_ok());
    let units = json!({"delay":{"days":1,"hours":1,"minutes":1,"seconds":1,"milliseconds":1}});
    assert_eq!(
        resolve_retry_policy(&raw, &units, "/retry", None)
            .unwrap()
            .delay_ms,
        90_061_001
    );
}
#[test]
fn every_ignored_field_and_nested_reference_form_is_rejected() {
    for policy in [
        json!({"when":"${ false }"}),
        json!({"exceptWhen":"${ true }"}),
        json!({"backoff":{}}),
        json!({"jitter":{}}),
        json!({"limit":{"duration":{"seconds":2}}}),
        json!({"limit":{"attempt":{"duration":{"seconds":2}}}}),
        json!({"use":"fixed"}),
        json!({"unknown":true}),
        json!({"limit":{"unknown":true}}),
        json!({"limit":{"attempt":{"unknown":true}}}),
        json!({"delay":{"unknown":1}}),
    ] {
        let inline = definition(policy.clone());
        assert_eq!(
            validate_retry_policies(&inline).unwrap_err().error.category,
            Category::Unsupported,
            "{policy}"
        );
        let mut reference = definition(json!("fixed"));
        reference["use"] = json!({"retries":{"fixed":policy}});
        assert!(validate_retry_policies(&reference).is_err());
        let mut nested = definition(json!({}));
        nested["do"] = json!([{"fork":{"fork":{"branches":[{"branch":{"call":"http","with":{},"retry":inline["do"][0]["fetch"]["retry"]}}]}}}]);
        assert!(validate_retry_policies(&nested).is_err());
    }
    for components in [
        json!({"alias":"fixed","fixed":supported()}),
        json!({"alias":{"use":"fixed"},"fixed":supported()}),
    ] {
        let mut raw = definition(json!("alias"));
        raw["use"] = json!({"retries":components});
        assert!(validate_retry_policies(&raw).is_err());
    }
    assert!(validate_retry_policies(&definition(json!("missing"))).is_err());
}
#[test]
fn invalid_counts_delays_and_overflow_fail_closed() {
    for policy in [
        json!(null),
        json!({"limit":null}),
        json!({"limit":{"attempt":{"count":0}}}),
        json!({"limit":{"attempt":{"count":65536}}}),
        json!({"limit":{"attempt":{"count":1.5}}}),
        json!({"limit":{"attempt":{"count":"3"}}}),
        json!({"delay":{"seconds":-1}}),
        json!({"delay":{"days":u64::MAX}}),
        json!({"delay":{"seconds":"${ context.delay }"}}),
        json!({"delay":null}),
    ] {
        assert!(
            validate_retry_policies(&definition(policy.clone())).is_err(),
            "{policy}"
        );
    }
}
#[test]
fn admission_rejects_policy_without_compiling_conditions_and_preserves_legacy() {
    let engine = Engine::new(WorkerConfig {
        workers: 1,
        ..WorkerConfig::default()
    })
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut raw = definition(json!({"when":"${ private_secret }"}));
    let plan = DefinitionValidation::from_raw(&raw).unwrap();
    match runtime.block_on(plan.validate(&engine)).unwrap_err() {
        ValidationError::Diagnostic(d) => {
            assert_eq!(d.error.category, Category::Unsupported);
            assert_eq!(d.field, "/do/0/fetch/retry/when");
        }
        _ => panic!("must fail admission"),
    }
    assert_eq!(engine.compilation_requests(), 0);
    raw["document"]["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("lightExpressionProfile");
    assert!(
        runtime
            .block_on(
                DefinitionValidation::from_raw(&raw)
                    .unwrap()
                    .validate(&engine)
            )
            .is_ok()
    );
    engine.shutdown(std::time::Duration::from_secs(2)).unwrap();
}
