use serde_json::{Value, json};
use serverless_core::StatusError;
#[test]
fn wp2_java_status_bytes_are_identical() {
    let capture: Value = serde_json::from_str(include_str!(
        "data/E-01-exact-status-bytes-cp1-e02-accepted.json"
    ))
    .unwrap();
    for (code, raw) in capture["observation"].as_object().unwrap() {
        let raw = raw.as_str().unwrap();
        let fields: Value = serde_json::from_str(raw).unwrap();
        let status = StatusError::new(
            fields["statusCode"].as_u64().unwrap() as u16,
            code,
            fields["message"].as_str().unwrap(),
            fields["description"].as_str().unwrap(),
        );
        assert_eq!(status.to_bytes().as_ref(), raw.as_bytes());
        let resp = status.response();
        assert_eq!(resp.status.as_u16(), status.status_code);
        assert_eq!(resp.headers["content-type"], "application/json");
        assert!(!resp.body_is_binary);
    }
}
#[test]
fn wp2_unescaped_strings_and_ordered_escaped_metadata_are_preserved() {
    let capture: Value = serde_json::from_str(include_str!(
        "data/E-01-status-metadata-escaping-cp1-e02-accepted.json"
    ))
    .unwrap();
    let mut status = StatusError::new(418, "TEST_ONLY", "quoted \"value\"", "snow雪\nnewline");
    status.metadata = Some(Box::new(vec![
        ("quoted".into(), json!("a\"b")),
        ("nullValue".into(), Value::Null),
    ]));
    assert_eq!(
        status.to_bytes().as_ref(),
        capture["observation"]["exactStatusBytesUtf8"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    assert!(serde_json::from_slice::<Value>(&status.to_bytes()).is_err());
}
#[test]
fn timeout_and_fail_closed_status_match_approved_fixtures() {
    assert_eq!(
        StatusError::timeout().to_bytes(),
        "{\"statusCode\":504,\"code\":\"ERR14017\",\"message\":\"BACKEND_TIMEOUT\",\"description\":\"Backend did not respond within the request deadline.\",\"severity\":\"ERROR\"}"
    );
    assert_eq!(
        StatusError::handler_failure().to_bytes(),
        "{\"statusCode\":500,\"code\":\"ERR14004\",\"message\":\"MIDDLEWARE_UNHANDLED_EXCEPTION\",\"description\":\"Middleware thread unhandled runtime exception.\",\"severity\":\"ERROR\"}"
    );
}
