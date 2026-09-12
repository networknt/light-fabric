//! Public projection of durable execution results. Never forward runner envelopes,
//! stderr, credentials or arbitrary worker metadata to the browser.
use serde_json::{Value, json};

pub(crate) fn result_message(turn_id: uuid::Uuid, state: &str, result: &Value) -> Value {
    let output = result.pointer("/result/structuredOutput");
    let text = output
        .and_then(|v| {
            v.pointer("/worker/finalMessage")
                .or_else(|| v.get("finalMessage"))
        })
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 1024 * 1024);
    let mut message = json!({"type":"executionResult", "turnId":turn_id, "state":state,
        "text": if state == "COMPLETED" { text } else { None }});
    if state == "COMPLETED"
        && let Some(workspace) = output.and_then(|v| v.get("workspace"))
    {
        message["workspace"] = json!({"workspaceId":workspace["workspaceId"],"taskId":workspace["taskId"],
            "checkpointDigest":workspace["checkpointDigest"]});
    }
    if state == "COMPLETED"
        && let Some(thread) = output.and_then(|v| v.get("codingThread"))
    {
        message["codingThread"] = json!({"sessionRef":thread["sessionRef"],"checkpoint":thread["checkpoint"],"state":thread["state"]});
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_only_successful_public_explanation() {
        let id = uuid::Uuid::nil();
        let value = json!({"result":{"structuredOutput":{"worker":{
            "finalMessage":"Changed the README", "authentication":{"secret":"hidden"}
        }},"stderr":"private diagnostic"}});
        assert_eq!(
            result_message(id, "COMPLETED", &value),
            json!({"type":"executionResult","turnId":id,"state":"COMPLETED","text":"Changed the README"})
        );
        assert!(result_message(id, "FAILED", &value)["text"].is_null());
        assert!(result_message(id, "COMPLETED", &Value::Null)["text"].is_null());
    }
}
