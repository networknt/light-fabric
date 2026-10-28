use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use serverless_aws::{
    alb::{IngressMode, encode},
    limits::Limits,
    v1::parse_response,
};
use serverless_core::GatewayResponse;
fn response(body: Vec<u8>) -> GatewayResponse {
    GatewayResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body: Some(body.into()),
        body_is_binary: false,
    }
}
fn parse(v: Value) -> Result<GatewayResponse, serverless_core::StatusError> {
    parse_response(
        &serde_json::to_vec(&v).unwrap(),
        None,
        None,
        "request",
        "FnX",
    )
}
#[test]
fn response_validation_overlap_and_repeated_cookies() {
    let r=parse(json!({"statusCode":201,"headers":{"Content-Type":"ignored","X-Scalar":"yes","SET-COOKIE":"ignored"},"multiValueHeaders":{"content-type":["text/plain"],"Set-Cookie":["a=1","b=2"]},"body":"ok"})).unwrap();
    assert_eq!(r.headers["content-type"], "text/plain");
    assert_eq!(r.headers["x-scalar"], "yes");
    assert_eq!(
        r.headers
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a=1", "b=2"]
    );
    let v: Value =
        serde_json::from_slice(&encode(&r, IngressMode::Multi, false, &Limits::default()).unwrap())
            .unwrap();
    assert_eq!(v["statusDescription"], "201 Created");
    assert_eq!(v["multiValueHeaders"]["set-cookie"], json!(["a=1", "b=2"]));
    for invalid in [
        json!(null),
        json!("text"),
        json!([]),
        json!([200, null, null, null, null]),
        json!({}),
        json!({"statusCode":99}),
        json!({"statusCode":600}),
        json!({"statusCode":200.5}),
        json!({"statusCode":"200"}),
        json!({"statusCode":200,"headers":{"bad name":"v"}}),
        json!({"statusCode":200,"multiValueHeaders":{"ok":["bad\n"]}}),
        json!({"statusCode":200,"body":"%%%","isBase64Encoded":true}),
    ] {
        assert!(parse(invalid.clone()).is_err(), "{invalid}");
    }
    assert!(parse_response(b"{bad", None, None, "request", "FnX").is_err());
}
#[test]
fn absent_null_empty_flags_binary_and_forced_encoding() {
    for flag in [
        None,
        Some(Value::Null),
        Some(json!(false)),
        Some(json!(true)),
    ] {
        for body in [None, Some(Value::Null), Some(json!(""))] {
            let mut v = json!({"statusCode":200});
            if let Some(flag) = &flag {
                v["isBase64Encoded"] = flag.clone();
            }
            if let Some(body) = &body {
                v["body"] = body.clone();
            }
            let r = parse(v).unwrap();
            assert_eq!(
                r.body.is_some(),
                body.as_ref().is_some_and(|b| !b.is_null())
            );
            let out: Value = serde_json::from_slice(
                &encode(&r, IngressMode::Multi, false, &Limits::default()).unwrap(),
            )
            .unwrap();
            assert_eq!(out["body"], body.clone().unwrap_or(Value::Null));
            assert_eq!(
                out["isBase64Encoded"],
                json!(r.body.is_some() && flag == Some(json!(true)))
            );
        }
    }
    for (body, binary, force, expected) in [
        (vec![255], false, false, "/w=="),
        (b"hi".to_vec(), true, false, "aGk="),
        (b"hi".to_vec(), false, true, "aGk="),
    ] {
        let mut r = response(body);
        r.body_is_binary = binary;
        let out: Value = serde_json::from_slice(
            &encode(&r, IngressMode::Multi, force, &Limits::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(out["body"], expected);
        assert_eq!(out["isBase64Encoded"], true);
    }
    let mut r = response(vec![]);
    r.status = StatusCode::from_u16(599).unwrap();
    let out: Value = serde_json::from_slice(
        &encode(&r, IngressMode::Single, false, &Limits::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(out["statusDescription"], "599 Unknown");
    assert!(out.get("headers").is_some());
}
#[test]
fn exact_boundary_and_one_byte_over_final_envelope() {
    let limits = Limits {
        max_response_bytes: 4096,
        ..Limits::default()
    };
    let overhead = encode(&response(vec![]), IngressMode::Multi, false, &limits)
        .unwrap()
        .len();
    let r = response(vec![b'a'; 4096 - overhead]);
    let bytes = encode(&r, IngressMode::Multi, false, &limits).unwrap();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["statusCode"],
        200
    );
    let r = response(vec![b'a'; 4097 - overhead]);
    let out: Value =
        serde_json::from_slice(&encode(&r, IngressMode::Multi, false, &limits).unwrap()).unwrap();
    assert_eq!(out["statusCode"], 502);
    assert!(out["body"].as_str().unwrap().contains("ERR14016"));
    assert!(out["body"].as_str().unwrap().contains("4096 bytes"));
    assert!(
        Limits {
            max_response_bytes: 4095,
            ..limits
        }
        .validate()
        .is_err()
    );
}
#[test]
fn headers_escaping_and_base64_are_counted_and_replacement_is_bounded() {
    let limits = Limits {
        max_response_bytes: 4096,
        ..Limits::default()
    };
    let mut header = response(vec![b'a'; 10]);
    header
        .headers
        .insert("x-large", "a".repeat(4100).parse().unwrap());
    header
        .headers
        .insert("x-correlation-id", "z".repeat(256).parse().unwrap());
    let escaping = response(vec![b'\n'; 2200]);
    let mut binary = response(vec![0xff; 3100]);
    binary.body_is_binary = true;
    for r in [header, escaping, binary] {
        assert!(r.body.as_ref().unwrap().len() < 4096);
        let bytes = encode(&r, IngressMode::Multi, false, &limits).unwrap();
        assert!(bytes.len() < 4096);
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["statusCode"], 502);
        assert_eq!(out["isBase64Encoded"], false);
        let h = out["multiValueHeaders"].as_object().unwrap();
        assert!(
            h.keys()
                .all(|k| k == "content-type" || k == "x-correlation-id")
        );
        assert!(h.len() <= 2);
    }
    let mut r = response(vec![b'a'; 5000]);
    r.headers
        .insert("x-correlation-id", "a".repeat(10000).parse().unwrap());
    let out: Value =
        serde_json::from_slice(&encode(&r, IngressMode::Multi, false, &limits).unwrap()).unwrap();
    assert!(out["multiValueHeaders"].get("x-correlation-id").is_none());
    let mut r = response(Vec::new());
    r.body = Some(Bytes::from_static(b""));
    assert!(
        !encode(&r, IngressMode::Multi, false, &limits)
            .unwrap()
            .is_empty()
    );
}
#[test]
fn function_error_is_not_http_success_or_payload_response() {
    for error in ["Handled", "Unhandled", "unknown"] {
        let e = parse_response(
            br#"{"statusCode":200,"body":"secret"}"#,
            Some(error),
            Some("7"),
            "request",
            "FnX",
        )
        .unwrap_err();
        assert_eq!(e.code, "ERR14014");
        assert!(
            !String::from_utf8(e.to_bytes().to_vec())
                .unwrap()
                .contains("secret")
        );
    }
}

#[test]
fn replacement_worst_case_correlation_escaping_remains_below_four_kib() {
    let limits = Limits {
        max_response_bytes: 4096,
        ..Limits::default()
    };
    let mut r = response(vec![b'x'; 5000]);
    r.headers
        .insert("x-correlation-id", "\t".repeat(256).parse().unwrap());
    for mode in [IngressMode::Single, IngressMode::Multi] {
        let bytes = encode(&r, mode, true, &limits).unwrap();
        assert!(bytes.len() < 4096);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["statusCode"], 502);
        assert_eq!(value["isBase64Encoded"], false);
        assert_eq!(
            value["body"].as_str().unwrap(),
            "{\"statusCode\":502,\"code\":\"ERR14016\",\"message\":\"RESPONSE_TOO_LARGE\",\"description\":\"Backend response exceeds the ingress limit of 4096 bytes.\",\"severity\":\"ERROR\"}"
        );
    }
}
