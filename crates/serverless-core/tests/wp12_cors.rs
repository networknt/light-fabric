pub mod support;
use serde_json::Value;
use serverless_core::{handlers::cors::CorsHandler, plan::*, *};
use std::sync::Arc;
use support::*;

fn build(yaml: &str, counts: &Arc<Counts>) -> Pipeline {
    let mut registry = Registry::default();
    registry.register(
        HandlerId::RequestCors,
        Arc::new(CorsHandler::from_yaml(serde_yaml::from_str(yaml).unwrap(), false).unwrap()),
    );
    registry.register(
        HandlerId::ResponseCors,
        Arc::new(CorsHandler::from_yaml(serde_yaml::from_str(yaml).unwrap(), true).unwrap()),
    );
    install(
        &mut registry,
        HandlerId::Proxy,
        Binding::Dispatch(
            TargetRoutes::compile([
                ("/*@get".into(), Target::Function("FnTest".into())),
                ("/*@options".into(), Target::Function("FnTest".into())),
            ])
            .unwrap(),
        ),
    );
    Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::RequestCors, "request"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseCors, "response"),
                ],
                &["request", "proxy", "response"],
            ),
            &registry,
        ),
        vec![counts.clone()],
    )
}
const GLOBAL: &str =
    "enabled: true\nallowedOrigins: [https://test.invalid]\nallowedMethods: [GET, POST]\n";
#[tokio::test]
async fn real_preflight_preserves_java_headers_null_body_and_dispatch() {
    let c = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.method = http::Method::OPTIONS;
    inv.headers
        .insert("origin", "https://test.invalid:443".parse().unwrap());
    inv.headers
        .insert("access-control-request-method", "DELETE".parse().unwrap());
    inv.body = Some(bytes::Bytes::from_static(b"backend response"));
    let result = build(GLOBAL, &c).execute(inv, &FakeBackend::new(&c)).await;
    assert_eq!(result.status, 200);
    assert_eq!(result.body, None);
    assert!(!result.body_is_binary);
    assert_eq!(
        result.headers["access-control-allow-origin"],
        "https://test.invalid:443"
    );
    assert_eq!(result.headers["access-control-allow-methods"], "GET,POST");
    assert_eq!(
        result.headers["access-control-allow-headers"],
        "Content-Type, WWW-Authenticate, Authorization"
    );
    assert_eq!(result.headers["access-control-allow-credentials"], "true");
    assert_eq!(result.headers["access-control-max-age"], "3600");
    assert_eq!(result.headers["vary"], "Origin");
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test]
async fn real_policies_use_approved_multimap_fixture_inputs_and_expectations() {
    for file in [
        "B-02a-denied-multi-policy-v1.json",
        "B-02a-allowed-multi-policy-v1.json",
    ] {
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/../../tests/serverless-conformance/fixtures/v1/b/{file}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
        .unwrap();
        let yaml = serde_yaml::to_string(&fixture["config"]["files"]["cors"]).unwrap();
        let step = &fixture["steps"][0];
        let mut inv = invocation();
        inv.headers.insert(
            "origin",
            step["rawEvent"]["multiValueHeaders"]["origin"][0]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
        let c = Arc::new(Counts::default());
        let result = build(&yaml, &c).execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(
            result.status.as_u16(),
            step["expect"]["status"].as_u64().unwrap() as u16
        );
        assert_eq!(
            n(&c.backend),
            step["expect"]["backendCalls"].as_array().unwrap().len()
        );
        if result.status == 403 {
            assert_eq!(
                result.body.as_ref().unwrap().as_ref(),
                step["expect"]["body"].as_str().unwrap().as_bytes()
            );
            assert!(!result.headers.contains_key("access-control-allow-origin"));
        } else {
            assert_eq!(
                result.headers["access-control-allow-origin"],
                "https://test.invalid"
            );
            assert!(!result.headers.contains_key("vary"));
            assert!(!result.headers.contains_key("access-control-allow-methods"));
        }
        assert_eq!(n(&c.finalized), 1);
    }
}
#[tokio::test]
async fn prefixes_use_first_declared_match_not_longest_and_preserve_missing_methods_failure() {
    let yaml = "enabled: true\nallowedOrigins: [https://global.invalid]\nallowedMethods: [GET]\npathPrefixAllowed:\n  /x:\n    allowedOrigins: [https://first.invalid]\n    allowedMethods: [POST]\n  /x/deep:\n    allowedOrigins: [https://second.invalid]\n    allowedMethods: [DELETE]\n";
    let c = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.path = "/x/deep".into();
    inv.headers
        .insert("origin", "https://first.invalid".parse().unwrap());
    assert_eq!(
        build(yaml, &c)
            .execute(inv, &FakeBackend::new(&c))
            .await
            .status,
        200
    );
    assert_eq!(n(&c.backend), 1);
    let c = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.method = http::Method::OPTIONS;
    inv.headers
        .insert("origin", "https://test.invalid".parse().unwrap());
    let result = build(
        "enabled: true\nallowedOrigins: [https://test.invalid]\n",
        &c,
    )
    .execute(inv, &FakeBackend::new(&c))
    .await;
    assert_eq!(result.status, 500);
    assert_eq!(n(&c.backend), 0);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test]
async fn origin_presence_only_no_same_host_fallback_and_explicit_request_headers() {
    for origin in [None, Some("https://not-allowed.invalid")] {
        let c = Arc::new(Counts::default());
        let mut inv = invocation();
        inv.headers
            .insert("host", "not-allowed.invalid".parse().unwrap());
        inv.headers
            .insert("access-control-request-method", "POST".parse().unwrap());
        if let Some(origin) = origin {
            inv.headers.insert("origin", origin.parse().unwrap());
        }
        let result = build(GLOBAL, &c).execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(result.status, if origin.is_some() { 403 } else { 200 });
        assert_eq!(n(&c.backend), usize::from(origin.is_none()));
    }
    let c = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.method = http::Method::OPTIONS;
    inv.headers
        .insert("origin", "https://test.invalid".parse().unwrap());
    inv.headers.insert(
        "access-control-request-headers",
        "X-Custom, authorization".parse().unwrap(),
    );
    let result = build(GLOBAL, &c).execute(inv, &FakeBackend::new(&c)).await;
    assert_eq!(
        result.headers["access-control-allow-headers"],
        "X-Custom, authorization"
    );
}
#[test]
fn strict_configuration_rejects_malformed_and_unknown_even_disabled() {
    assert!(
        CorsHandler::from_yaml(
            serde_yaml::from_str(
                "enabled: true\nallowedOrigins: ''\nallowedMethods: ''\npathPrefixAllowed: ''\n"
            )
            .unwrap(),
            false
        )
        .is_ok()
    );
    for yaml in [
        "enabled: nope",
        "enabled: false\nextra: true",
        "allowedMethods: GET",
        "allowedOrigins: [1]",
        "pathPrefixAllowed: []",
        "pathPrefixAllowed: {x: {}}",
        "pathPrefixAllowed: {/x: {allowedOrigins: true}}",
        "pathPrefixAllowed: {/x: {extra: 1}}",
    ] {
        assert!(
            CorsHandler::from_yaml(serde_yaml::from_str(yaml).unwrap(), false).is_err(),
            "{yaml}"
        );
    }
}
#[test]
fn java_origin_normalization_is_byte_exact_and_does_not_normalize_allowed_values() {
    use gateway_policy::java_cors::*;
    for (raw, expected) in [
        ("https://test.invalid:443", "https://test.invalid"),
        ("http://[::1]:80", "http://[::1]"),
        ("HTTPS://test.invalid:443", "HTTPS://test.invalid:443"),
        ("https://test.invalid/path", "https://test.invalid/path"),
        ("null", "null"),
    ] {
        assert_eq!(sanitize_default_port(raw).unwrap(), expected);
    }
    assert!(sanitize_default_port("https://test.invalid:443/path").is_err());
    assert!(
        !origin_allowed(
            "https://test.invalid:443",
            &["https://test.invalid:443".into()]
        )
        .unwrap()
    );
    assert!(origin_allowed("https://TEST.invalid:443", &["https://test.invalid".into()]).unwrap());
    assert!(!origin_allowed("https://test.invalid", &["*".into()]).unwrap());
}
