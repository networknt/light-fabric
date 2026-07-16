use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::{Mutex, oneshot};

use crate::client::RegistryHandler;
use crate::protocol::JsonRpcMessage;

pub(crate) type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcMessage>>>>;

pub(crate) async fn handle_inbound_message(
    handler: &Arc<dyn RegistryHandler>,
    pending_requests: &PendingRequests,
    message: JsonRpcMessage,
) -> Option<JsonRpcMessage> {
    if let Some(method) = message.method.as_deref() {
        if message.id.is_some() {
            let result = handler
                .handle_request(method, message.params.clone().unwrap_or(json!({})))
                .await;
            return Some(JsonRpcMessage {
                jsonrpc: "2.0".to_string(),
                id: message.id,
                method: None,
                params: None,
                result: Some(result),
                error: None,
            });
        }
        handler
            .handle_notification(method, message.params.unwrap_or(json!({})))
            .await;
        return None;
    }

    if let Some(id) = message.id.as_ref() {
        let id = match id {
            serde_json::Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        if let Some(response_tx) = pending_requests.lock().await.remove(&id) {
            let _ = response_tx.send(message);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoHandler;

    #[async_trait::async_trait]
    impl RegistryHandler for EchoHandler {
        async fn handle_request(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> serde_json::Value {
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

    #[test]
    fn shared_legacy_json_fixture_is_accepted_by_portal_protocol() {
        let fixture: serde_json::Value =
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
