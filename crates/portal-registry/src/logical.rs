use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};

use crate::client::RegistryHandler;
use crate::protocol::{JsonRpcError, JsonRpcMessage};

#[derive(Debug, Clone)]
pub(crate) enum RuntimeSessionInput {
    Request {
        request_id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        request_id: String,
        response: RuntimeResponse,
    },
    Ignored,
}

#[derive(Debug, Clone)]
pub(crate) enum RuntimeSessionOutput {
    Request {
        request_id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        request_id: Value,
        response: RuntimeResponse,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeResponse {
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

pub(crate) type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<RuntimeResponse>>>>;

pub(crate) fn input_from_legacy_json(message: JsonRpcMessage) -> RuntimeSessionInput {
    if let Some(method) = message.method {
        let params = message.params.unwrap_or_else(|| json!({}));
        return match message.id {
            Some(request_id) => RuntimeSessionInput::Request {
                request_id,
                method,
                params,
            },
            None => RuntimeSessionInput::Notification { method, params },
        };
    }

    if let Some(id) = message.id {
        let request_id = match &id {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        return RuntimeSessionInput::Response {
            request_id,
            response: RuntimeResponse {
                result: message.result,
                error: message.error,
            },
        };
    }

    RuntimeSessionInput::Ignored
}

pub(crate) fn output_to_legacy_json(output: RuntimeSessionOutput) -> JsonRpcMessage {
    match output {
        RuntimeSessionOutput::Request {
            request_id,
            method,
            params,
        } => JsonRpcMessage::new_request(request_id, &method, params),
        RuntimeSessionOutput::Notification { method, params } => {
            JsonRpcMessage::new_notification(&method, params)
        }
        RuntimeSessionOutput::Response {
            request_id,
            response,
        } => JsonRpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(request_id),
            method: None,
            params: None,
            result: response.result,
            error: response.error,
        },
    }
}

pub(crate) async fn handle_inbound(
    handler: &Arc<dyn RegistryHandler>,
    pending_requests: &PendingRequests,
    input: RuntimeSessionInput,
) -> Option<RuntimeSessionOutput> {
    match input {
        RuntimeSessionInput::Request {
            request_id,
            method,
            params,
        } => {
            let result = handler.handle_request(&method, params).await;
            Some(RuntimeSessionOutput::Response {
                request_id,
                response: RuntimeResponse {
                    result: Some(result),
                    error: None,
                },
            })
        }
        RuntimeSessionInput::Notification { method, params } => {
            handler.handle_notification(&method, params).await;
            None
        }
        RuntimeSessionInput::Response {
            request_id,
            response,
        } => {
            if let Some(response_tx) = pending_requests.lock().await.remove(&request_id) {
                let _ = response_tx.send(response);
            }
            None
        }
        RuntimeSessionInput::Ignored => None,
    }
}

#[allow(dead_code)] // Kept as the characterized legacy adapter entry point for N1.
pub(crate) async fn handle_inbound_message(
    handler: &Arc<dyn RegistryHandler>,
    pending_requests: &PendingRequests,
    message: JsonRpcMessage,
) -> Option<JsonRpcMessage> {
    handle_inbound(handler, pending_requests, input_from_legacy_json(message))
        .await
        .map(output_to_legacy_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;

    #[async_trait::async_trait]
    impl RegistryHandler for EchoHandler {
        async fn handle_request(&self, method: &str, params: Value) -> Value {
            json!({"method": method, "params": params})
        }
    }

    #[tokio::test]
    async fn request_characterization_preserves_id_and_result_shape() {
        let handler: Arc<dyn RegistryHandler> = Arc::new(EchoHandler);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let response = handle_inbound_message(
            &handler,
            &pending,
            JsonRpcMessage::new_request(json!("request-1"), "controller/check", json!({"a": 1})),
        )
        .await
        .expect("response");

        assert_eq!(response.id, Some(json!("request-1")));
        assert_eq!(
            response.result,
            Some(json!({"method": "controller/check", "params": {"a": 1}}))
        );
    }

    #[tokio::test]
    async fn logical_dispatch_matches_legacy_dispatch() {
        let handler: Arc<dyn RegistryHandler> = Arc::new(EchoHandler);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let output = handle_inbound(
            &handler,
            &pending,
            RuntimeSessionInput::Request {
                request_id: json!(17),
                method: "controller/check".to_string(),
                params: json!({"a": 1}),
            },
        )
        .await
        .expect("response");

        assert_eq!(
            serde_json::to_string(&output_to_legacy_json(output)).unwrap(),
            r#"{"jsonrpc":"2.0","id":17,"method":null,"result":{"method":"controller/check","params":{"a":1}}}"#
        );
    }

    #[test]
    fn shared_legacy_json_fixture_is_accepted_by_portal_protocol() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/controller-session-v1.json"))
                .expect("golden fixture");
        for name in [
            "registration",
            "registrationAcknowledgement",
            "metadataUpdate",
            "commandRequest",
            "commandResponse",
            "notification",
        ] {
            serde_json::from_value::<JsonRpcMessage>(fixture[name].clone())
                .unwrap_or_else(|error| panic!("invalid {name} fixture: {error}"));
        }
    }
}
