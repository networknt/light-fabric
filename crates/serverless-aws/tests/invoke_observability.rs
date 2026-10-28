mod support;
use axum::{
    Router,
    body::Bytes,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use serverless_aws::{invoke::InvokeOptions, limits::Limits};
use serverless_core::{Backend, ExchangeState, Target};
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::*;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
struct Wire {
    id: Option<&'static str>,
    function_error: bool,
    application_status: u16,
    calls: AtomicUsize,
}
async fn respond(State(w): State<Arc<Wire>>, request: Request) -> Response {
    w.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(request.headers()["x-amz-log-type"], "None");
    assert_eq!(
        request.headers()["x-amz-invocation-type"],
        "RequestResponse"
    );
    let body = if w.function_error {
        json!({"errorMessage":"PRIVATE_BACKEND_ERROR","errorType":"PRIVATE_TYPE/disallowed","stackTrace":["PRIVATE_STACK"]})
    } else {
        json!({"statusCode":w.application_status,"body":"PRIVATE_BUSINESS_BODY","headers":{"x-test":"retained"},"isBase64Encoded":false})
    };
    let mut response = (http::StatusCode::OK, Bytes::from(body.to_string())).into_response();
    if let Some(id) = w.id {
        response
            .headers_mut()
            .insert("x-amzn-requestid", id.parse().unwrap());
    }
    if w.function_error {
        response
            .headers_mut()
            .insert("x-amz-function-error", "Unhandled".parse().unwrap());
    }
    // Unsolicited log tails must remain suppressed by the existing SDK wrapper.
    response
        .headers_mut()
        .insert("x-amz-log-result", "PRIVATE_LOG_TAIL".parse().unwrap());
    response
}

#[tokio::test]
async fn sdk_api_ids_are_bounded_metadata_not_execution_ids_and_do_not_change_responses_or_dispatch()
 {
    let writer = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_current_span(true)
            .with_span_list(true)
            .with_writer(writer.clone())
            .finish(),
    )
    .unwrap();
    let valid = "ab123456-1234-4abc-8def-123456789abc";
    let uppercase = "AB123456-1234-4ABC-8DEF-123456789ABC";
    for (id, status, function_error, expected_id) in [
        (Some(valid), 201, false, Some(valid)),
        (Some(uppercase), 500, false, Some(uppercase)),
        (Some(valid), 200, true, Some(valid)),
        (None, 200, false, None),
        (Some("PRIVATE_NOT_UUID"), 200, false, None),
        (
            Some("ab123456_1234-4abc-8def-123456789abc"),
            200,
            false,
            None,
        ),
        (
            Some("gb123456-1234-4abc-8def-123456789abc"),
            200,
            false,
            None,
        ),
        (
            Some("ab123456-1234-4abc-8def-123456789abc-PRIVATE_SUFFIX"),
            200,
            false,
            None,
        ),
    ] {
        let wire = Arc::new(Wire {
            id,
            function_error,
            application_status: status,
            calls: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(respond).with_state(wire.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let span = tracing::info_span!(
            "private_caller",
            authorization = "PRIVATE_SPAN_TOKEN",
            payload = "PRIVATE_SPAN_BODY"
        );
        let offset = writer.0.lock().unwrap().len();
        let b = backend(&endpoint, InvokeOptions::default(), Limits::default());
        let mut request = event();
        request["body"] = json!("PRIVATE_REQUEST_BODY");
        let result = tracing::Instrument::instrument(
            b.dispatch(
                &invocation(request),
                &ExchangeState::default(),
                &Target::Function("FnX".into()),
            ),
            span,
        )
        .await;
        if function_error {
            assert_eq!(result.unwrap_err().0.code, "ERR14014");
        } else {
            let result = result.unwrap();
            assert_eq!(result.status.as_u16(), status);
            assert_eq!(
                result.body.as_deref(),
                Some(b"PRIVATE_BUSINESS_BODY".as_slice())
            );
            assert!(
                result
                    .headers
                    .iter()
                    .any(|(k, v)| k == "x-test" && v == "retained")
            );
        }
        assert_eq!(wire.calls.load(Ordering::SeqCst), 1);
        task.abort();
        let _ = task.await;
        let text = String::from_utf8(writer.0.lock().unwrap()[offset..].to_vec()).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let metadata: Vec<_> = rows
            .iter()
            .filter(|r| r["fields"]["event"] == "invoke_response")
            .collect();
        assert_eq!(metadata.len(), 1, "{text}");
        let row = metadata[0];
        assert_eq!(row["fields"]["request_id"], "test-only-request");
        assert_eq!(row["fields"]["target_alias"], "FnX");
        assert_eq!(
            row["fields"]
                .get("backend_invoke_api_request_id")
                .and_then(Value::as_str),
            expected_id
        );
        assert_eq!(row["span"], json!({"name":"safe_diagnostic"}), "{row}");
        assert_eq!(row["spans"], json!([{"name":"safe_diagnostic"}]), "{row}");
        assert!(!row.to_string().contains("PRIVATE_"), "{row}");
        assert!(row["fields"].get("backend_execution_id").is_none());
        if let Some(id) = id.filter(|_| expected_id.is_none()) {
            // Raw SDK dependency diagnostics are filtered by the application
            // privacy wrapper, tested separately. This assertion covers our
            // newly emitted typed observation without replacing that wrapper.
            assert!(
                !row.to_string().contains(id),
                "malformed API ID reached typed observation"
            );
        }
        for forbidden in [
            "PRIVATE_REQUEST_BODY",
            "PRIVATE_BUSINESS_BODY",
            "PRIVATE_LOG_TAIL",
            "PRIVATE_BACKEND_ERROR",
            "PRIVATE_STACK",
            "PRIVATE_TYPE",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} in logs");
        }
    }
}
