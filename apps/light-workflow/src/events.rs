use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

fn deserialize_string_or_number_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "expected string or number, got {other}"
            )));
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudEventEnvelope {
    pub specversion: String,
    pub id: String,
    pub source: String,
    pub r#type: String, // "type" is a reserved word in Rust
    pub subject: Option<String>,
    pub time: Option<DateTime<Utc>>,
    pub datacontenttype: Option<String>,
    pub data: Option<Value>,
    // Extensions
    pub user: Option<String>,
    pub host: Option<String>,
    #[serde(deserialize_with = "deserialize_string_or_number_opt", default)]
    pub nonce: Option<String>,
    pub aggregatetype: Option<String>,
    #[serde(alias = "aggregateversion")]
    #[serde(deserialize_with = "deserialize_string_or_number_opt", default)]
    pub eventaggregateversion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStartedPayload {
    pub host_id: Uuid,
    pub wf_def_id: Uuid,
    pub wf_instance_id: Option<Uuid>,
    #[serde(default)]
    pub input: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::WorkflowStartedPayload;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn workflow_started_payload_reads_json_input_and_instance_id() {
        let host_id = Uuid::new_v4();
        let wf_def_id = Uuid::new_v4();
        let wf_instance_id = Uuid::new_v4();
        let value = json!({
            "hostId": host_id,
            "wfDefId": wf_def_id,
            "wfInstanceId": wf_instance_id,
            "input": {
                "applicantId": "APP-001"
            }
        });

        let payload: WorkflowStartedPayload =
            serde_json::from_value(value).expect("payload should deserialize");

        assert_eq!(payload.host_id, host_id);
        assert_eq!(payload.wf_def_id, wf_def_id);
        assert_eq!(payload.wf_instance_id, Some(wf_instance_id));
        assert_eq!(
            payload.input,
            Some(json!({
                "applicantId": "APP-001"
            }))
        );
    }
}
