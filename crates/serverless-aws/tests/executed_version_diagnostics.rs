use serde_json::{Value, json};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn absent_version_is_null_and_present_versions_keep_original_privacy_policy() {
    let bytes = Arc::new(Mutex::new(vec![]));
    let capture = Capture(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || capture.clone())
        .finish();
    let payload = serde_json::to_vec(&json!({"errorType":"TestOnly.Error","errorMessage":"FORBIDDEN_ERROR_MESSAGE","stackTrace":["FORBIDDEN_STACK"],"body":"FORBIDDEN_PAYLOAD"})).unwrap();
    let long = "x".repeat(129);
    let versions = [
        None,
        Some("7"),
        Some("$LATEST"),
        Some("safe_alias-7"),
        Some("FORBIDDEN_SECRET/value"),
        Some(long.as_str()),
    ];
    tracing::subscriber::with_default(subscriber, || {
        for version in versions {
            assert!(
                serverless_aws::v1::parse_response(
                    &payload,
                    Some("Handled"),
                    version,
                    "test-request",
                    "function0"
                )
                .is_err()
            );
        }
    });
    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    for forbidden in [
        "FORBIDDEN_ERROR_MESSAGE",
        "FORBIDDEN_STACK",
        "FORBIDDEN_PAYLOAD",
        "FORBIDDEN_SECRET",
        "logTail",
        "logResult",
    ] {
        assert!(!logs.contains(forbidden));
    }
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), versions.len());
    for (i, event) in events.iter().enumerate() {
        let metadata: Value =
            serde_json::from_str(event["fields"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(event["fields"].as_object().unwrap().len(), 1);
        assert_eq!(metadata.as_object().unwrap().len(), 6);
        let expected = match i {
            0 => Value::Null,
            1 => json!("7"),
            2 => json!("$LATEST"),
            3 => json!("safe_alias-7"),
            _ => json!("<redacted>"),
        };
        assert_eq!(metadata["executedVersion"], expected);
        assert!(
            metadata
                .as_object()
                .unwrap()
                .contains_key("executedVersion")
        );
        assert_eq!(
            metadata,
            json!({"requestId":"test-request","targetAlias":"function0","functionError":"Handled","executedVersion":expected,"payloadBytes":payload.len(),"errorType":"TestOnly.Error"})
        );
    }
}
