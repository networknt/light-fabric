mod support;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use serverless_aws::{
    alb::{self, IngressMode},
    limits::Limits,
    v1,
};
use serverless_core::ExchangeState;
use support::*;

#[test]
fn classify_and_match_single_value_error_shape() {
    for other in [
        json!({"version":"2.0"}),
        json!({"Records":[]}),
        json!({"requestContext":{"elb":{}}}),
    ] {
        let e = alb::decode(other, context(), &Limits::default()).unwrap_err();
        assert_eq!(e.status.code, "ERR14010");
    }
    let mut e = event();
    e.as_object_mut().unwrap().remove("multiValueHeaders");
    e["headers"] = json!({});
    let error = alb::decode(e, context(), &Limits::default()).unwrap_err();
    assert_eq!(error.mode, IngressMode::Single);
    assert_eq!(error.status.code, "ERR14011");
    let out: Value = serde_json::from_slice(
        &alb::encode(
            &error.status.response(),
            error.mode,
            false,
            &Limits::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(out.get("headers").is_some());
    assert!(out.get("multiValueHeaders").is_none());
}
#[test]
fn once_decoding_order_xff_and_original_preserved() {
    let mut e = event();
    e["path"] = json!("/a%2Fb/%252F");
    e["multiValueQueryStringParameters"] =
        json!({"b":["%2B","+","%252F"],"%61":["first","second"]});
    e["multiValueHeaders"] = json!({"X-Mixed":["1","2"],"x-forwarded-for":["198.51.100.1, 203.0.113.9","192.0.2.1, 203.0.113.17"]});
    let inv = invocation(e.clone());
    assert_eq!(inv.original, e);
    assert_eq!(inv.raw_path, "/a%2Fb/%252F");
    assert_eq!(inv.path, "/a/b/%2F");
    assert_eq!(
        inv.raw_query.as_deref(),
        Some("%61=first&%61=second&b=%2B&b=+&b=%252F")
    );
    assert_eq!(
        inv.query.0,
        vec![
            ("a".into(), "first".into()),
            ("a".into(), "second".into()),
            ("b".into(), "+".into()),
            ("b".into(), "+".into()),
            ("b".into(), "%2F".into())
        ]
    );
    assert_eq!(inv.headers.get_all("x-mixed").iter().count(), 2);
    assert_eq!(inv.source.source_ip.unwrap().to_string(), "203.0.113.17");
    for bad in ["198.51.100.1, invalid", "198.51.100.1,"] {
        let mut e = event();
        e["multiValueHeaders"] = json!({"x-forwarded-for":[bad]});
        assert!(invocation(e).source.source_ip.is_none());
    }
}
#[test]
fn absent_null_empty_and_base64_round_trip() {
    for body in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("YQ==")),
    ] {
        for encoded in [false, true] {
            let mut e = event();
            if let Some(body) = &body {
                e["body"] = body.clone();
            } else {
                e.as_object_mut().unwrap().remove("body");
            }
            e["isBase64Encoded"] = json!(encoded);
            let inv = invocation(e);
            let v = v1::build_v1_event(&inv, &ExchangeState::default(), &cfg(), time()).unwrap();
            let expected = body.as_ref().filter(|b| !b.is_null());
            assert_eq!(v["body"], expected.cloned().unwrap_or(Value::Null));
            assert_eq!(v["isBase64Encoded"], json!(encoded && expected.is_some()));
            let proxy =
                json!({"statusCode":200,"body":v["body"],"isBase64Encoded":v["isBase64Encoded"]});
            let r = v1::parse_response(
                &serde_json::to_vec(&proxy).unwrap(),
                None,
                None,
                "request",
                "FnX",
            )
            .unwrap();
            assert_eq!(r.body, inv.body);
        }
    }
    let mut e = event();
    e["body"] = json!(STANDARD.encode([0xff, 0, 0x80]));
    e["isBase64Encoded"] = json!(true);
    assert_eq!(invocation(e).body.unwrap().as_ref(), [0xff, 0, 0x80]);
}
#[test]
fn invalid_encoding_and_decoded_byte_limits() {
    for (key, value) in [
        ("httpMethod", json!("G ET")),
        ("httpMethod", json!("")),
        ("path", json!("/%ff")),
        ("path", json!("/%2")),
        ("path", json!("/%GG")),
        ("multiValueHeaders", json!({"bad header":["v"]})),
        ("multiValueHeaders", json!({"ok":["bad\r\nvalue"]})),
        ("multiValueQueryStringParameters", json!({"%ff":["v"]})),
    ] {
        let mut e = event();
        e[key] = value;
        assert_eq!(
            alb::decode(e, context(), &Limits::default())
                .unwrap_err()
                .status
                .code,
            "ERR14012"
        );
    }
    let mut e = event();
    e["isBase64Encoded"] = json!(true);
    e["body"] = json!("not base64");
    assert_eq!(
        alb::decode(e, context(), &Limits::default())
            .unwrap_err()
            .status
            .code,
        "ERR14012"
    );
    let limits = Limits {
        max_request_bytes: 4,
        ..Limits::default()
    };
    for encoded in [false, true] {
        for (body, accepted) in [("éé", true), ("ééx", false)] {
            let mut e = event();
            e["body"] = json!(if encoded {
                STANDARD.encode(body)
            } else {
                body.into()
            });
            e["isBase64Encoded"] = json!(encoded);
            assert_eq!(alb::decode(e, context(), &limits).is_ok(), accepted);
        }
    }
}
