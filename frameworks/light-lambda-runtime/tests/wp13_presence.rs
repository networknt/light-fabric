//! Presence is observed from the production builder, before CEL evaluation.
use light_lambda_runtime::transformer_adapter::{Fault, observe_response_context};
use serde_json::{Value, json};
use serverless_core::*;
use std::time::Duration;
fn invocation(event: Value) -> Invocation {
    let now = tokio::time::Instant::now();
    serverless_aws::alb::decode(
        event,
        serverless_aws::alb::DecodeContext {
            request_id: "presence".into(),
            deadline: now + Duration::from_secs(2),
            response_deadline: now + Duration::from_secs(3),
            cancel: tokio_util::sync::CancellationToken::new(),
        },
        &serverless_aws::limits::Limits::default(),
    )
    .unwrap()
}
#[tokio::test]
async fn absent_null_empty_and_populated_maps_preserve_presence_and_exact_budget() {
    for mode in [
        "absent",
        "null",
        "empty",
        "populated",
        "multivalue-empty",
        "routed",
    ] {
        let mut event = json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","multiValueHeaders":{},"body":""});
        match mode {
            "null" => {
                event["queryStringParameters"] = Value::Null;
                event["pathParameters"] = Value::Null;
            }
            "empty" => {
                event["queryStringParameters"] = json!({});
                event["pathParameters"] = json!({});
            }
            "multivalue-empty" => {
                event["multiValueQueryStringParameters"] = json!({});
                event["pathParameters"] = json!({});
            }
            "populated" => {
                event["multiValueQueryStringParameters"] = json!({"a":["first","last"]});
                event["pathParameters"] = json!({"id":"42"});
            }
            _ => {}
        }
        let mut inv = invocation(event);
        // Path matching publishes typed path params even when absent in ingress.
        if matches!(mode, "populated" | "routed") {
            inv.path_params.insert("id".into(), "42".into());
            if mode == "routed" {
                inv.query.0.push(("a".into(), "first".into()));
            }
        }
        let state = ExchangeState::default();
        let mut response = GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::new()),
            body_is_binary: false,
        };
        let input = observe_response_context(&inv, &state, &response).unwrap();
        let absent = matches!(mode, "absent" | "null");
        assert_eq!(input["queryParameters"].is_null(), absent, "{mode}");
        assert_eq!(input["pathParameters"].is_null(), absent, "{mode}");
        if matches!(mode, "populated" | "routed") {
            assert_eq!(input["queryParameters"]["a"], "first");
            assert_eq!(input["pathParameters"]["id"], "42");
        }
        let base = serde_json::to_vec(&input).unwrap().len();
        response.body = Some(bytes::Bytes::from(vec![b'x'; 32768 - base]));
        assert_eq!(
            serde_json::to_vec(&observe_response_context(&inv, &state, &response).unwrap())
                .unwrap()
                .len(),
            32768,
            "{mode}"
        );
        response.body = Some(bytes::Bytes::from(vec![b'x'; 32768 - base + 1]));
        assert_eq!(
            observe_response_context(&inv, &state, &response),
            Err(Fault::InputProfile),
            "{mode}"
        );
        // Rejection occurs without mutating borrowed caller data.
        assert_eq!(inv.body.as_deref(), Some(b"".as_slice()));
        assert_eq!(response.body.as_ref().unwrap().len(), 32768 - base + 1);
    }
}
