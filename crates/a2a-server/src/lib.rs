//! Strict A2A JSON-RPC server models shared by native and integration runtimes.
//!
//! The module owns wire validation and response vocabulary. Authentication,
//! authorization, durable admission, and business execution remain with the
//! embedding runtime.

use a2a_core::{TaskSnapshot, TaskState};
use a2a_protocol::{A2aOperation, ProtocolVersion};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SendMessageInput {
    pub message_id: String,
    pub context_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub text: String,
    pub return_immediately: bool,
    pub history_length: Option<usize>,
    pub metadata: Value,
}

#[derive(Debug, Clone)]
pub struct TaskInput {
    pub task_id: Uuid,
    pub history_length: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ListTasksInput {
    pub context_id: Option<Uuid>,
    pub status: Option<TaskState>,
    pub page_size: usize,
    pub page_token: Option<String>,
    pub include_artifacts: bool,
    pub history_length: Option<usize>,
    pub status_timestamp_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageCursor {
    pub created_at: DateTime<Utc>,
    pub task_id: Uuid,
}

#[derive(Debug, Clone)]
pub enum OperationInput {
    Send(SendMessageInput),
    Task(TaskInput),
    List(ListTasksInput),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServerError {
    #[error("Invalid params")]
    InvalidParams,
    #[error("UnsupportedOperationError")]
    UnsupportedOperation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendMessageRequest {
    #[serde(default)]
    tenant: Option<String>,
    message: Message,
    #[serde(default)]
    configuration: Option<SendConfiguration>,
    #[serde(default = "empty_object")]
    metadata: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendConfiguration {
    #[serde(default)]
    accepted_output_modes: Vec<String>,
    #[serde(default)]
    history_length: Option<i32>,
    #[serde(default)]
    return_immediately: bool,
    #[serde(default)]
    blocking: Option<bool>,
    #[serde(default)]
    push_notification_config: Option<Value>,
    #[serde(default)]
    task_push_notification_config: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Message {
    message_id: String,
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    role: String,
    parts: Vec<Part>,
    #[serde(default = "empty_object")]
    metadata: Value,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    reference_task_ids: Vec<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default = "empty_object")]
    metadata: Value,
    #[serde(default)]
    filename: Option<String>,
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskRequest {
    #[serde(default)]
    tenant: Option<String>,
    id: String,
    #[serde(default)]
    history_length: Option<i32>,
    #[serde(default = "empty_object")]
    metadata: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListRequest {
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    page_size: Option<i32>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    history_length: Option<i32>,
    #[serde(default)]
    status_timestamp_after: Option<String>,
    #[serde(default)]
    include_artifacts: bool,
}

pub fn parse_operation(
    operation: A2aOperation,
    version: ProtocolVersion,
    params: Value,
    maximum_text_bytes: usize,
) -> Result<OperationInput, ServerError> {
    match operation {
        A2aOperation::SendMessage | A2aOperation::SendStreamingMessage => {
            let request: SendMessageRequest =
                serde_json::from_value(params).map_err(|_| ServerError::InvalidParams)?;
            if request.tenant.as_deref().is_some_and(str::is_empty)
                || version == ProtocolVersion::V03 && request.tenant.is_some()
                || request.message.message_id.trim().is_empty()
                || request.message.message_id.len() > 256
                || !request.message.extensions.is_empty()
                || !request.message.reference_task_ids.is_empty()
                || !request.message.metadata.is_object()
                || !request.metadata.is_object()
                || !matches!(
                    (version, request.message.role.as_str()),
                    (ProtocolVersion::V10, "ROLE_USER") | (ProtocolVersion::V03, "user")
                )
                || !matches!(
                    (version, request.message.kind.as_deref()),
                    (ProtocolVersion::V10, None) | (ProtocolVersion::V03, Some("message"))
                )
            {
                return Err(ServerError::InvalidParams);
            }
            let configuration = request.configuration.unwrap_or(SendConfiguration {
                accepted_output_modes: Vec::new(),
                history_length: None,
                return_immediately: false,
                blocking: None,
                push_notification_config: None,
                task_push_notification_config: None,
            });
            if configuration
                .accepted_output_modes
                .iter()
                .any(|mode| mode.trim().is_empty())
                || configuration.history_length.is_some_and(|value| value < 0)
                || configuration.push_notification_config.is_some()
                || configuration.task_push_notification_config.is_some()
                || matches!(version, ProtocolVersion::V10) && configuration.blocking.is_some()
                || version == ProtocolVersion::V03 && configuration.return_immediately
            {
                return Err(ServerError::InvalidParams);
            }
            let return_immediately = match version {
                ProtocolVersion::V10 => configuration.return_immediately,
                ProtocolVersion::V03 => !configuration.blocking.unwrap_or(false),
            };
            let mut text = None;
            for part in request.message.parts {
                let content_count = usize::from(part.text.is_some())
                    + usize::from(part.raw.is_some())
                    + usize::from(part.url.is_some())
                    + usize::from(part.data.is_some());
                if content_count != 1
                    || part.raw.is_some()
                    || part.url.is_some()
                    || part.data.is_some()
                    || !part.metadata.is_object()
                    || part.filename.is_some()
                    || part
                        .media_type
                        .as_deref()
                        .is_some_and(|mode| mode != "text/plain")
                    || !matches!(
                        (version, part.kind.as_deref()),
                        (ProtocolVersion::V10, None) | (ProtocolVersion::V03, Some("text"))
                    )
                    || text.is_some()
                {
                    return Err(ServerError::InvalidParams);
                }
                text = part.text;
            }
            let text = text
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty() && value.len() <= maximum_text_bytes)
                .ok_or(ServerError::InvalidParams)?;
            let context_id = optional_uuid(request.message.context_id)?;
            let task_id = optional_uuid(request.message.task_id)?;
            Ok(OperationInput::Send(SendMessageInput {
                message_id: request.message.message_id,
                context_id,
                task_id,
                text,
                return_immediately,
                history_length: optional_usize(configuration.history_length)?,
                metadata: request.metadata,
            }))
        }
        A2aOperation::GetTask | A2aOperation::CancelTask | A2aOperation::SubscribeToTask => {
            let request: TaskRequest =
                serde_json::from_value(params).map_err(|_| ServerError::InvalidParams)?;
            if request.tenant.as_deref().is_some_and(str::is_empty)
                || version == ProtocolVersion::V03 && request.tenant.is_some()
                || request.history_length.is_some_and(|value| value < 0)
                || operation != A2aOperation::GetTask && request.history_length.is_some()
                || !request.metadata.is_object()
            {
                return Err(ServerError::InvalidParams);
            }
            Ok(OperationInput::Task(TaskInput {
                task_id: Uuid::parse_str(&request.id).map_err(|_| ServerError::InvalidParams)?,
                history_length: optional_usize(request.history_length)?,
            }))
        }
        A2aOperation::ListTasks => {
            let request: ListRequest =
                serde_json::from_value(params).map_err(|_| ServerError::InvalidParams)?;
            let page_size = request.page_size.unwrap_or(50);
            if request.tenant.as_deref().is_some_and(str::is_empty)
                || request.history_length.is_some_and(|value| value < 0)
                || !(1..=100).contains(&page_size)
            {
                return Err(ServerError::InvalidParams);
            }
            Ok(OperationInput::List(ListTasksInput {
                context_id: optional_uuid(request.context_id)?,
                status: optional_task_state(request.status.as_deref())?,
                page_size: page_size as usize,
                page_token: request.page_token.filter(|value| !value.is_empty()),
                include_artifacts: request.include_artifacts,
                history_length: optional_usize(request.history_length)?,
                status_timestamp_after: request
                    .status_timestamp_after
                    .map(|value| {
                        DateTime::parse_from_rfc3339(&value)
                            .map(|value| value.with_timezone(&Utc))
                            .map_err(|_| ServerError::InvalidParams)
                    })
                    .transpose()?,
            }))
        }
        A2aOperation::GetAgentCard
        | A2aOperation::GetExtendedAgentCard
        | A2aOperation::CreateTaskPushNotificationConfig
        | A2aOperation::GetTaskPushNotificationConfig
        | A2aOperation::ListTaskPushNotificationConfigs
        | A2aOperation::DeleteTaskPushNotificationConfig => Err(ServerError::UnsupportedOperation),
    }
}

fn optional_uuid(value: Option<String>) -> Result<Option<Uuid>, ServerError> {
    value
        .map(|value| Uuid::parse_str(&value).map_err(|_| ServerError::InvalidParams))
        .transpose()
}

fn empty_object() -> Value {
    json!({})
}

fn optional_usize(value: Option<i32>) -> Result<Option<usize>, ServerError> {
    value
        .map(|value| usize::try_from(value).map_err(|_| ServerError::InvalidParams))
        .transpose()
}

fn optional_task_state(value: Option<&str>) -> Result<Option<TaskState>, ServerError> {
    match value {
        None | Some("") | Some("TASK_STATE_UNSPECIFIED") => Ok(None),
        Some("TASK_STATE_SUBMITTED") => Ok(Some(TaskState::Submitted)),
        Some("TASK_STATE_WORKING") => Ok(Some(TaskState::Working)),
        Some("TASK_STATE_COMPLETED") => Ok(Some(TaskState::Completed)),
        Some("TASK_STATE_FAILED") => Ok(Some(TaskState::Failed)),
        Some("TASK_STATE_CANCELED") => Ok(Some(TaskState::Canceled)),
        Some("TASK_STATE_INPUT_REQUIRED") => Ok(Some(TaskState::InputRequired)),
        Some("TASK_STATE_REJECTED") => Ok(Some(TaskState::Rejected)),
        Some("TASK_STATE_AUTH_REQUIRED") => Ok(Some(TaskState::AuthRequired)),
        Some(_) => Err(ServerError::InvalidParams),
    }
}

pub fn decode_page_token(value: Option<&str>) -> Result<Option<PageCursor>, ServerError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > 1024 {
        return Err(ServerError::InvalidParams);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ServerError::InvalidParams)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| ServerError::InvalidParams)
}

pub fn encode_page_token(cursor: Option<PageCursor>) -> Result<Option<String>, ServerError> {
    cursor
        .map(|cursor| {
            serde_json::to_vec(&cursor)
                .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                .map_err(|_| ServerError::InvalidParams)
        })
        .transpose()
}

pub fn task_value(snapshot: &TaskSnapshot, version: ProtocolVersion, now: DateTime<Utc>) -> Value {
    task_value_with_history(snapshot, version, now, None)
}

pub fn task_value_with_history(
    snapshot: &TaskSnapshot,
    version: ProtocolVersion,
    now: DateTime<Utc>,
    history_length: Option<usize>,
) -> Value {
    let artifacts = snapshot
        .artifacts
        .iter()
        .map(|artifact| {
            let mut value = json!({
                "artifactId": artifact.artifact_id,
                "name": artifact.logical_name,
                "parts": [{
                    "data": {
                        "contentDigest": artifact.content_digest,
                        "sizeBytes": artifact.size_bytes,
                        "provenanceDigest": artifact.provenance_digest
                    },
                    "mediaType": artifact.media_type
                }],
                "metadata": {"retentionDeadline": artifact.retention_deadline}
            });
            if version == ProtocolVersion::V03 {
                value["parts"][0]["kind"] = json!("data");
            }
            value
        })
        .collect::<Vec<_>>();
    let history = snapshot
        .result
        .as_ref()
        .and_then(|result| result.get("text"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| {
            let value = match version {
                ProtocolVersion::V10 => json!({
                    "messageId": format!("{}-result", snapshot.task_id),
                    "contextId": snapshot.context_id,
                    "taskId": snapshot.task_id,
                    "role": "ROLE_AGENT",
                    "parts": [{"text": text}],
                    "metadata": {},
                    "extensions": [],
                    "referenceTaskIds": []
                }),
                ProtocolVersion::V03 => json!({
                    "kind": "message",
                    "messageId": format!("{}-result", snapshot.task_id),
                    "contextId": snapshot.context_id,
                    "taskId": snapshot.task_id,
                    "role": "agent",
                    "parts": [{"kind":"text","text": text}],
                    "metadata": {}
                }),
            };
            vec![value]
        })
        .unwrap_or_default();
    let history = if history_length == Some(0) {
        Vec::new()
    } else {
        history
    };
    let mut value = json!({
        "id": snapshot.task_id,
        "contextId": snapshot.context_id,
        "status": {
            "state": wire_state(snapshot.state, version),
            "timestamp": now
        },
        "artifacts": artifacts,
        "history": history,
        "metadata": {}
    });
    if version == ProtocolVersion::V03 {
        value["kind"] = json!("task");
    }
    value
}

pub fn send_result(snapshot: &TaskSnapshot, version: ProtocolVersion, now: DateTime<Utc>) -> Value {
    send_result_with_history(snapshot, version, now, None)
}

pub fn send_result_with_history(
    snapshot: &TaskSnapshot,
    version: ProtocolVersion,
    now: DateTime<Utc>,
    history_length: Option<usize>,
) -> Value {
    let task = task_value_with_history(snapshot, version, now, history_length);
    match version {
        ProtocolVersion::V10 => json!({"task": task}),
        ProtocolVersion::V03 => task,
    }
}

pub fn list_result(
    tasks: &[TaskSnapshot],
    version: ProtocolVersion,
    page_size: usize,
    total_size: usize,
    next_page_token: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    list_result_with_history(
        tasks,
        version,
        page_size,
        total_size,
        next_page_token,
        now,
        None,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn list_result_with_history(
    tasks: &[TaskSnapshot],
    version: ProtocolVersion,
    page_size: usize,
    total_size: usize,
    next_page_token: Option<&str>,
    now: DateTime<Utc>,
    history_length: Option<usize>,
    include_artifacts: bool,
) -> Value {
    let tasks = tasks
        .iter()
        .map(|task| {
            let mut value = task_value_with_history(task, version, now, history_length);
            if !include_artifacts {
                value
                    .as_object_mut()
                    .expect("Task is an object")
                    .remove("artifacts");
            }
            value
        })
        .collect::<Vec<_>>();
    json!({
        "tasks": tasks,
        "nextPageToken": next_page_token.unwrap_or(""),
        "pageSize": page_size,
        "totalSize": total_size
    })
}

pub fn status_stream_result(
    snapshot: &TaskSnapshot,
    version: ProtocolVersion,
    now: DateTime<Utc>,
) -> Value {
    match version {
        ProtocolVersion::V10 => json!({
            "statusUpdate": {
                "taskId": snapshot.task_id,
                "contextId": snapshot.context_id,
                "status": {"state": wire_state(snapshot.state, version), "timestamp": now},
                "metadata": {}
            }
        }),
        ProtocolVersion::V03 => json!({
            "kind": "status-update",
            "taskId": snapshot.task_id,
            "contextId": snapshot.context_id,
            "status": {"state": wire_state(snapshot.state, version), "timestamp": now},
            "final": snapshot.state.terminal(),
            "metadata": {}
        }),
    }
}

pub const fn wire_state(state: TaskState, version: ProtocolVersion) -> &'static str {
    match (version, state) {
        (ProtocolVersion::V10, TaskState::Submitted) => "TASK_STATE_SUBMITTED",
        (ProtocolVersion::V10, TaskState::Working) => "TASK_STATE_WORKING",
        (ProtocolVersion::V10, TaskState::InputRequired) => "TASK_STATE_INPUT_REQUIRED",
        (ProtocolVersion::V10, TaskState::AuthRequired) => "TASK_STATE_AUTH_REQUIRED",
        (ProtocolVersion::V10, TaskState::Completed) => "TASK_STATE_COMPLETED",
        (ProtocolVersion::V10, TaskState::Failed) => "TASK_STATE_FAILED",
        (ProtocolVersion::V10, TaskState::Canceled) => "TASK_STATE_CANCELED",
        (ProtocolVersion::V10, TaskState::Rejected) => "TASK_STATE_REJECTED",
        (ProtocolVersion::V03, TaskState::Submitted) => "submitted",
        (ProtocolVersion::V03, TaskState::Working) => "working",
        (ProtocolVersion::V03, TaskState::InputRequired) => "input-required",
        (ProtocolVersion::V03, TaskState::AuthRequired) => "auth-required",
        (ProtocolVersion::V03, TaskState::Completed) => "completed",
        (ProtocolVersion::V03, TaskState::Failed) => "failed",
        (ProtocolVersion::V03, TaskState::Canceled) => "canceled",
        (ProtocolVersion::V03, TaskState::Rejected) => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a_core::Direction;

    #[test]
    fn pinned_v1_message_uses_client_message_id_and_server_task_shape() {
        let input = parse_operation(
            A2aOperation::SendMessage,
            ProtocolVersion::V10,
            json!({"message":{"messageId":"m-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}],"metadata":{},"extensions":[],"referenceTaskIds":[]},"configuration":{"returnImmediately":true},"metadata":{}}),
            64,
        )
        .unwrap();
        assert!(
            matches!(input, OperationInput::Send(SendMessageInput { message_id, return_immediately: true, .. }) if message_id == "m-1")
        );
        let task = TaskSnapshot {
            task_id: Uuid::now_v7(),
            context_id: Uuid::now_v7(),
            state: TaskState::Working,
            direction: Direction::Inbound,
            target_agent_ref: "account.agent".into(),
            result: None,
            error: None,
            artifacts: Vec::new(),
        };
        let value = send_result(&task, ProtocolVersion::V10, Utc::now());
        assert_eq!(value["task"]["status"]["state"], "TASK_STATE_WORKING");
    }

    #[test]
    fn terminal_task_exposes_the_agent_response_without_an_artifact_download_url() {
        let task = TaskSnapshot {
            task_id: Uuid::now_v7(),
            context_id: Uuid::now_v7(),
            state: TaskState::Completed,
            direction: Direction::Inbound,
            target_agent_ref: "account.agent".into(),
            result: Some(json!({"text":"Account 42 is active"})),
            error: None,
            artifacts: Vec::new(),
        };
        let value = task_value(&task, ProtocolVersion::V10, Utc::now());
        assert_eq!(value["history"][0]["role"], "ROLE_AGENT");
        assert_eq!(
            value["history"][0]["parts"][0]["text"],
            "Account 42 is active"
        );
        assert!(value.to_string().find("agent-turn-result:").is_none());
        let list = list_result_with_history(
            std::slice::from_ref(&task),
            ProtocolVersion::V10,
            50,
            1,
            None,
            Utc::now(),
            Some(0),
            false,
        );
        assert_eq!(list["tasks"][0]["history"], json!([]));
        assert!(list["tasks"][0].get("artifacts").is_none());

        let cursor = PageCursor {
            created_at: Utc::now(),
            task_id: task.task_id,
        };
        let token = encode_page_token(Some(cursor)).unwrap().unwrap();
        let decoded = decode_page_token(Some(&token)).unwrap().unwrap();
        assert_eq!(decoded.task_id, cursor.task_id);
        assert_eq!(decoded.created_at, cursor.created_at);
    }

    #[test]
    fn pinned_v03_message_and_task_discriminators_are_enforced() {
        let task_id = Uuid::now_v7();
        let input = parse_operation(
            A2aOperation::SendStreamingMessage,
            ProtocolVersion::V03,
            json!({"message":{"kind":"message","messageId":"m-2","taskId":task_id,"role":"user","parts":[{"kind":"text","text":"continue"}],"metadata":{},"extensions":[],"referenceTaskIds":[]},"metadata":{}}),
            64,
        )
        .unwrap();
        assert!(
            matches!(input, OperationInput::Send(SendMessageInput { task_id: Some(value), return_immediately: true, .. }) if value == task_id)
        );
        let blocking = parse_operation(
            A2aOperation::SendMessage,
            ProtocolVersion::V03,
            json!({"message":{"kind":"message","messageId":"m-3","role":"user","parts":[{"kind":"text","text":"wait"}]},"configuration":{"blocking":true}}),
            64,
        )
        .unwrap();
        assert!(matches!(
            blocking,
            OperationInput::Send(SendMessageInput {
                return_immediately: false,
                ..
            })
        ));
        assert_eq!(
            wire_state(TaskState::Canceled, ProtocolVersion::V03),
            "canceled"
        );
        let task = TaskSnapshot {
            task_id,
            context_id: Uuid::now_v7(),
            state: TaskState::Completed,
            direction: Direction::Inbound,
            target_agent_ref: "account.agent".into(),
            result: Some(json!({"text":"done"})),
            error: None,
            artifacts: Vec::new(),
        };
        let value = task_value(&task, ProtocolVersion::V03, Utc::now());
        assert_eq!(value["kind"], "task");
        assert_eq!(value["history"][0]["kind"], "message");
        assert_eq!(value["history"][0]["parts"][0]["kind"], "text");
    }
}
