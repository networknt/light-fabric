mod support;
use bytes::Bytes;
use http::Method;
use serde_json::json;
use serverless_aws::v1::{self, EventConfig};
use serverless_core::{ExchangeState, Principal, RouteMatch};
use std::collections::BTreeMap;
use support::*;

#[test]
fn selected_correlation_scalar_does_not_resurrect_or_undo_later_mutations() {
    let mut inv = invocation(event());
    inv.headers
        .append("x-correlation-id", "first".parse().unwrap());
    inv.headers
        .append("x-correlation-id", "second".parse().unwrap());
    let mut state = ExchangeState::default();
    state.correlation_id = Some("first".into());
    let built = v1::build_v1_event(&inv, &state, &cfg(), time()).unwrap();
    assert_eq!(built["headers"]["x-correlation-id"], "first");
    assert_eq!(
        built["multiValueHeaders"]["x-correlation-id"],
        json!(["first", "second"])
    );
    inv.headers.remove("x-correlation-id");
    let built = v1::build_v1_event(&inv, &state, &cfg(), time()).unwrap();
    assert!(built["headers"].get("x-correlation-id").is_none());
    inv.headers
        .insert("x-correlation-id", "later".parse().unwrap());
    assert_eq!(
        v1::build_v1_event(&inv, &state, &cfg(), time()).unwrap()["headers"]["x-correlation-id"],
        "later"
    );
}

#[test]
fn every_mapping_uses_finalized_request_and_explicit_context() {
    let mut raw = event();
    raw["body"] = json!("old");
    raw["requestContext"]["authorizer"] = json!({"claims":{"sub":"spoof"}});
    raw["multiValueHeaders"] =
        json!({"x-amzn-oidc-identity":["spoof"],"user-agent":["old","final-agent"]});
    let mut inv = invocation(raw);
    inv.method = Method::PATCH;
    inv.path = "/changed/42".into();
    inv.body = Some(Bytes::from_static(b"final-body"));
    inv.headers.append("x-final", "1".parse().unwrap());
    inv.headers.append("x-final", "2".parse().unwrap());
    inv.query.0 = vec![
        ("id".into(), "original".into()),
        ("q".into(), "1".into()),
        ("q".into(), "2".into()),
    ];
    inv.path_params.insert("id".into(), "42".into());
    inv.route = Some(RouteMatch {
        template: "/changed/{id}".into(),
        target: None,
    });
    inv.source.source_ip = Some("203.0.113.7".parse().unwrap());
    let mut cfg = cfg();
    cfg.account_id = Some("000000000000".into());
    cfg.api_id = Some("api".into());
    cfg.domain_name = Some("example.test".into());
    cfg.resource_id = Some("resource".into());
    cfg.stage_variables = Some(BTreeMap::from([("x".into(), "y".into())]));
    let v = v1::build_v1_event(&inv, &ExchangeState::default(), &cfg, time()).unwrap();
    assert_eq!(v["resource"], "/changed/{id}");
    assert_eq!(v["path"], "/changed/42");
    assert_eq!(v["httpMethod"], "PATCH");
    assert_eq!(v["body"], "final-body");
    assert_eq!(v["isBase64Encoded"], false);
    assert_eq!(v["headers"]["x-final"], "2");
    assert_eq!(v["multiValueHeaders"]["x-final"], json!(["1", "2"]));
    assert_eq!(v["queryStringParameters"], json!({"id":"42","q":"2"}));
    assert_eq!(
        v["multiValueQueryStringParameters"],
        json!({"id":["original"],"q":["1","2"]})
    );
    assert_eq!(v["pathParameters"], json!({"id":"42"}));
    assert_eq!(v["stageVariables"], json!({"x":"y"}));
    let c = &v["requestContext"];
    assert_eq!(c["requestId"], inv.request_id);
    assert_eq!(c["extendedRequestId"], inv.request_id);
    assert_eq!(c["resourcePath"], v["resource"]);
    assert_eq!(c["httpMethod"], "PATCH");
    assert_eq!(c["path"], v["path"]);
    assert_eq!(c["stage"], "test-only");
    assert_eq!(c["protocol"], "HTTP/1.1");
    assert_eq!(c["requestTime"], "14/Nov/2023:22:13:20 +0000");
    assert_eq!(c["requestTimeEpoch"], 1700000000123_i64);
    assert_eq!(c["accountId"], "000000000000");
    assert_eq!(c["apiId"], "api");
    assert_eq!(c["domainName"], "example.test");
    assert_eq!(c["resourceId"], "resource");
    assert_eq!(c["identity"]["sourceIp"], "203.0.113.7");
    assert_eq!(c["identity"]["userAgent"], "final-agent");
    for (k, v) in c["identity"].as_object().unwrap() {
        if k != "sourceIp" && k != "userAgent" {
            assert!(v.is_null());
        }
    }
    assert!(c.get("authorizer").is_none());
    assert!(v.get("version").is_none());
    assert!(c.get("elb").is_none());
}
#[test]
fn verified_principal_mapping_is_not_authentication_evidence() {
    let mut state = ExchangeState::default();
    let claims = json!({"sub":"verified","scope":["read","write"],"custom":{"unchanged":1}});
    state.principal = Some(Principal::from_verified(
        "verified".into(),
        None,
        None,
        claims.clone(),
    ));
    let v = v1::build_v1_event(&invocation(event()), &state, &cfg(), time()).unwrap();
    assert_eq!(
        v["requestContext"]["authorizer"],
        json!({"principalId":"verified","claims":claims,"scope":"read write"})
    );
}
#[test]
fn null_maps_stage_and_encoding_controls() {
    assert!(EventConfig::new("".into()).is_err());
    assert!(EventConfig::new("  ".into()).is_err());
    let mut inv = invocation(event());
    let v = v1::build_v1_event(&inv, &ExchangeState::default(), &cfg(), time()).unwrap();
    for key in [
        "headers",
        "multiValueHeaders",
        "queryStringParameters",
        "multiValueQueryStringParameters",
        "pathParameters",
        "stageVariables",
        "body",
    ] {
        assert!(v[key].is_null(), "{key}");
    }
    for key in ["accountId", "apiId", "domainName", "resourceId"] {
        assert!(v["requestContext"][key].is_null());
    }
    assert_eq!(v["resource"], "/x");
    let mut cfg = cfg();
    cfg.encode_base64_request = true;
    for (body, expected) in [
        (None, json!(null)),
        (Some(Bytes::new()), json!("")),
        (Some(Bytes::from_static(b"text")), json!("dGV4dA==")),
        (Some(Bytes::from_static(&[255])), json!("/w==")),
    ] {
        inv.body = body;
        let v = v1::build_v1_event(&inv, &ExchangeState::default(), &cfg, time()).unwrap();
        assert_eq!(v["body"], expected);
        assert_eq!(v["isBase64Encoded"], !expected.is_null());
    }
}

#[test]
fn configured_empty_stage_map_and_verified_claim_precedence() {
    let mut config = cfg();
    config.stage_variables = Some(BTreeMap::new());
    let inv = invocation(event());
    assert!(v1::build_v1_event(&inv,&ExchangeState::default(),&config,time()).unwrap()["stageVariables"].is_null());
    for claims in [
        json!({"sub":"first","cid":"second","client_id":"third","user_id":"fourth"}),
        json!({"cid":"first","client_id":"second"}),
        json!({"client_id":"first","user_id":"second"}),
        json!({"user_id":"first"}),
    ] {
        let mut state = ExchangeState::default();
        state.principal = Some(Principal::from_verified(
            "fallback".into(),
            None,
            None,
            claims,
        ));
        assert_eq!(
            v1::build_v1_event(&inv, &state, &config, time()).unwrap()["requestContext"]["authorizer"]
                ["principalId"],
            "first"
        );
    }
}

#[test]
fn outgoing_event_preserves_utf8_header_values_and_rejects_invalid_utf8() {
    let mut inv = invocation(event());
    inv.headers.append(
        "x-test-header",
        http::HeaderValue::from_bytes("é".as_bytes()).unwrap(),
    );
    inv.headers.append(
        "x-test-header",
        http::HeaderValue::from_bytes("終".as_bytes()).unwrap(),
    );
    let built = v1::build_v1_event(&inv, &ExchangeState::default(), &cfg(), time()).unwrap();
    assert_eq!(built["headers"]["x-test-header"], "終");
    assert_eq!(
        built["multiValueHeaders"]["x-test-header"],
        json!(["é", "終"])
    );
    inv.headers.insert(
        "x-test-header",
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    assert!(v1::build_v1_event(&inv, &ExchangeState::default(), &cfg(), time()).is_err());
}
