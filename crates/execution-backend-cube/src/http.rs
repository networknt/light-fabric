use super::{CubeApi, CubeCommandResult, CubeCreateRequest, CubeResource, CubeState};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_backend::BackendError;
use execution_runner_protocol::{ArtifactEvidence, CommandExecutionSpec};
use futures_util::StreamExt;
use prost::Message;
use reqwest::{StatusCode, header};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use url::Url;

const ENVD_PORT: u16 = 49_983;

#[derive(Debug, Clone)]
pub struct CubeHttpClientConfig {
    pub api_url: Url,
    pub sandbox_url: Option<Url>,
    pub api_key: String,
    pub request_timeout: Duration,
    pub maximum_response_bytes: usize,
    pub allow_insecure_http: bool,
    pub tls_ca_pem: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct CubeHttpClient {
    http: reqwest::Client,
    api_url: Url,
    sandbox_url: Option<Url>,
    api_key: header::HeaderValue,
    maximum_response_bytes: usize,
}

impl CubeHttpClient {
    pub fn new(config: CubeHttpClientConfig) -> Result<Self, String> {
        validate_url("Cube API", &config.api_url, config.allow_insecure_http)?;
        if let Some(url) = &config.sandbox_url {
            validate_url("Cube sandbox", url, config.allow_insecure_http)?;
        }
        if config.api_key.is_empty() || config.api_key.contains(char::is_whitespace) {
            return Err("Cube API key must be one non-empty token".into());
        }
        if config.request_timeout.is_zero() || config.maximum_response_bytes == 0 {
            return Err("Cube HTTP timeout and response limit must be positive".into());
        }
        let mut api_key = header::HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(|error| format!("invalid Cube API key header: {error}"))?;
        api_key.set_sensitive(true);
        let mut builder = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = config.tls_ca_pem {
            let certificate = reqwest::Certificate::from_pem(&pem)
                .map_err(|error| format!("invalid Cube TLS CA PEM: {error}"))?;
            builder = builder.add_root_certificate(certificate);
        }
        let http = builder
            .build()
            .map_err(|error| format!("build Cube HTTP client: {error}"))?;
        Ok(Self {
            http,
            api_url: normalized_base(config.api_url),
            sandbox_url: config.sandbox_url.map(normalized_base),
            api_key,
            maximum_response_bytes: config.maximum_response_bytes,
        })
    }

    fn api(&self, path: &str) -> Result<Url, BackendError> {
        self.api_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| BackendError::InvalidRequest(format!("invalid Cube URL: {error}")))
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(header::AUTHORIZATION, self.api_key.clone())
    }

    async fn detail(&self, id: &str) -> Result<Option<SandboxDetail>, BackendError> {
        let url = self.api(&format!("sandboxes/{id}"))?;
        let response = self
            .authorized(self.http.get(url))
            .send()
            .await
            .map_err(transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            decode_json(response, self.maximum_response_bytes).await?,
        ))
    }

    async fn list(
        &self,
        metadata: &BTreeMap<String, String>,
    ) -> Result<Vec<ListedSandbox>, BackendError> {
        let mut url = self.api("sandboxes")?;
        if !metadata.is_empty() {
            let value = metadata
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&");
            url.query_pairs_mut().append_pair("metadata", &value);
        }
        let response = self
            .authorized(self.http.get(url))
            .send()
            .await
            .map_err(transport)?;
        decode_json(response, self.maximum_response_bytes).await
    }

    fn envd_url(&self, detail: &SandboxDetail) -> Result<Url, BackendError> {
        if let Some(base) = &self.sandbox_url {
            return base
                .join("process.Process/Start")
                .map_err(|error| BackendError::InvalidRequest(error.to_string()));
        }
        let domain = detail.domain.as_deref().ok_or_else(|| {
            BackendError::Unsupported(
                "Cube detail omitted domain and sandboxUrl is not configured".into(),
            )
        })?;
        Url::parse(&format!(
            "https://{ENVD_PORT}-{}.{domain}/process.Process/Start",
            detail.sandbox_id
        ))
        .map_err(|error| BackendError::InvalidRequest(format!("invalid Cube envd URL: {error}")))
    }
}

#[async_trait]
impl CubeApi for CubeHttpClient {
    async fn create(&self, request: CubeCreateRequest) -> Result<CubeResource, BackendError> {
        if !request.deny_all_egress || request.credentials_enabled {
            return Err(BackendError::InvalidRequest(
                "Cube HTTP client requires deny-all egress and forbids credentials".into(),
            ));
        }
        if !request.inputs.is_empty() {
            return Err(BackendError::Unsupported(
                "Cube HTTP API cannot mount runner-local staged input paths; use an approved remote materializer".into(),
            ));
        }
        let now = Utc::now();
        let ttl = request
            .expires_at
            .signed_duration_since(now)
            .num_seconds()
            .max(1);
        let timeout = i32::try_from(ttl).map_err(|_| {
            BackendError::InvalidRequest("Cube native TTL does not fit i32 seconds".into())
        })?;
        let body = serde_json::json!({
            "templateID": request.template_id,
            "timeout": timeout,
            "lifecycle": {"onTimeout": "kill", "autoResume": false},
            "secure": true,
            "allow_internet_access": false,
            "network": {
                "allowPublicTraffic": false,
                "allowOut": [],
                "denyOut": ["0.0.0.0/0", "::/0"]
            },
            "metadata": request.tags,
            "envVars": {}
        });
        let response = self
            .authorized(self.http.post(self.api("sandboxes")?).json(&body))
            .send()
            .await
            .map_err(transport)?;
        let created: CreatedSandbox = decode_json(response, self.maximum_response_bytes).await?;
        Ok(CubeResource {
            environment_id: created.sandbox_id,
            idempotency_key: request.idempotency_key,
            state: CubeState::Ready,
            expires_at: request.expires_at,
            tags: request.tags,
        })
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<CubeResource>, BackendError> {
        let filter = BTreeMap::from([("light.idempotency".into(), key.into())]);
        let mut found = self.list(&filter).await?;
        if found.len() > 1 {
            return Err(BackendError::Unknown(
                "Cube returned multiple resources for one idempotency key".into(),
            ));
        }
        Ok(found.pop().map(|item| item.into_resource(key)))
    }

    async fn inspect(&self, environment_id: &str) -> Result<Option<CubeResource>, BackendError> {
        Ok(self.detail(environment_id).await?.map(|item| {
            let key = item
                .metadata
                .get("light.idempotency")
                .cloned()
                .unwrap_or_default();
            item.into_resource(key)
        }))
    }

    async fn execute(
        &self,
        environment_id: &str,
        command: &CommandExecutionSpec,
    ) -> Result<CubeCommandResult, BackendError> {
        let detail = self
            .detail(environment_id)
            .await?
            .ok_or_else(|| BackendError::NotFound(environment_id.into()))?;
        let request = StartRequest {
            process: Some(ProcessConfig {
                cmd: command.executable.clone(),
                args: command.arguments.clone(),
                envs: command.environment.clone().into_iter().collect(),
                cwd: Some(command.working_directory.clone()),
            }),
            stdin: Some(false),
        };
        let mut protobuf = Vec::new();
        request
            .encode(&mut protobuf)
            .map_err(|error| BackendError::InvalidRequest(error.to_string()))?;
        let mut framed = Vec::with_capacity(protobuf.len() + 5);
        framed.push(0);
        framed.extend_from_slice(&(protobuf.len() as u32).to_be_bytes());
        framed.extend_from_slice(&protobuf);
        let started_at = Utc::now();
        let mut builder = self
            .http
            .post(self.envd_url(&detail)?)
            .header(header::CONTENT_TYPE, "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .header("E2b-Sandbox-Id", environment_id)
            .header("E2b-Sandbox-Port", ENVD_PORT.to_string())
            .body(framed);
        if let Some(token) = &detail.envd_access_token {
            builder = builder.header("X-Access-Token", token);
        }
        let response = builder.send().await.map_err(transport)?;
        let bytes = bounded_body(response, self.maximum_response_bytes).await?;
        let stdout_limit = usize::try_from(command.stdout_limit_bytes).map_err(|_| {
            BackendError::InvalidRequest("Cube stdout limit does not fit this runner".into())
        })?;
        let stderr_limit = usize::try_from(command.stderr_limit_bytes).map_err(|_| {
            BackendError::InvalidRequest("Cube stderr limit does not fit this runner".into())
        })?;
        let (exit_code, stdout, stderr) = decode_start_stream(&bytes, stdout_limit, stderr_limit)?;
        Ok(CubeCommandResult {
            exit_code,
            stdout,
            stderr,
            started_at,
            finished_at: Utc::now(),
            evidence: BTreeMap::from([
                ("cubeProtocol".into(), "connect-proto-v1".into()),
                ("cubeSandboxId".into(), environment_id.into()),
            ]),
        })
    }

    async fn set_timeout(
        &self,
        environment_id: &str,
        timeout_seconds: u64,
    ) -> Result<(), BackendError> {
        let timeout = i32::try_from(timeout_seconds).map_err(|_| {
            BackendError::InvalidRequest("Cube timeout does not fit i32 seconds".into())
        })?;
        let response = self
            .authorized(
                self.http
                    .post(self.api(&format!("sandboxes/{environment_id}/timeout"))?)
                    .json(&serde_json::json!({"timeout": timeout})),
            )
            .send()
            .await
            .map_err(transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response).await)
        }
    }

    async fn cancel(&self, environment_id: &str) -> Result<(), BackendError> {
        self.delete(environment_id).await
    }

    async fn artifacts(&self, _: &str) -> Result<Vec<ArtifactEvidence>, BackendError> {
        Err(BackendError::Unsupported(
            "Cube artifact export is not configured".into(),
        ))
    }

    async fn delete(&self, environment_id: &str) -> Result<(), BackendError> {
        let response = self
            .authorized(
                self.http
                    .delete(self.api(&format!("sandboxes/{environment_id}"))?),
            )
            .send()
            .await
            .map_err(transport)?;
        if response.status() == StatusCode::NOT_FOUND || response.status().is_success() {
            return Ok(());
        }
        Err(response_error(response).await)
    }

    async fn discover_owned(
        &self,
        owner_runner: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<CubeResource>, Option<String>), BackendError> {
        if cursor.is_some() {
            return Err(BackendError::InvalidRequest(
                "Cube v1 metadata listing does not accept a cursor".into(),
            ));
        }
        if limit == 0 || limit > 200 {
            return Err(BackendError::InvalidRequest(
                "Cube discovery limit must be between 1 and 200".into(),
            ));
        }
        let filter = BTreeMap::from([("light.runner".into(), owner_runner.into())]);
        let mut resources = self
            .list(&filter)
            .await?
            .into_iter()
            .map(|item| {
                let key = item
                    .metadata
                    .get("light.idempotency")
                    .cloned()
                    .unwrap_or_default();
                item.into_resource(&key)
            })
            .collect::<Vec<_>>();
        if resources.len() > limit {
            return Err(BackendError::Unknown(
                "Cube owned-resource response exceeded the configured bound".into(),
            ));
        }
        resources.sort_by(|left, right| left.environment_id.cmp(&right.environment_id));
        Ok((resources, None))
    }
}

fn validate_url(name: &str, url: &Url, allow_insecure_http: bool) -> Result<(), String> {
    if url.cannot_be_a_base()
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "{name} URL must be an absolute base URL without credentials, query, or fragment"
        ));
    }
    if url.scheme() != "https" && !(allow_insecure_http && url.scheme() == "http") {
        return Err(format!("{name} URL must use HTTPS"));
    }
    Ok(())
}

fn normalized_base(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

fn transport(error: reqwest::Error) -> BackendError {
    BackendError::Transport(error.to_string())
}

async fn decode_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<T, BackendError> {
    let bytes = bounded_body(response, maximum_bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| BackendError::Transport(format!("decode Cube response: {error}")))
}

async fn bounded_body(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, BackendError> {
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if body.len().saturating_add(chunk.len()) > maximum_bytes {
            return Err(BackendError::Transport(
                "Cube response exceeded configured byte limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn response_error(response: reqwest::Response) -> BackendError {
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_default();
    let detail = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let message = format!("Cube HTTP {status}: {detail}");
    if status == StatusCode::NOT_FOUND {
        BackendError::NotFound(message)
    } else if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        BackendError::Transport(message)
    } else {
        BackendError::InvalidRequest(message)
    }
}

#[derive(Debug, Deserialize)]
struct CreatedSandbox {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
}

#[derive(Debug, Deserialize)]
struct ListedSandbox {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(rename = "endAt")]
    end_at: Option<DateTime<Utc>>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    state: String,
}

impl ListedSandbox {
    fn into_resource(self, idempotency_key: &str) -> CubeResource {
        CubeResource {
            environment_id: self.sandbox_id,
            idempotency_key: idempotency_key.into(),
            state: map_cube_state(&self.state),
            expires_at: self.end_at.unwrap_or_else(Utc::now),
            tags: self.metadata,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SandboxDetail {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(rename = "endAt")]
    end_at: Option<DateTime<Utc>>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    state: String,
    domain: Option<String>,
    #[serde(rename = "envdAccessToken")]
    envd_access_token: Option<String>,
}

impl SandboxDetail {
    fn into_resource(self, idempotency_key: String) -> CubeResource {
        CubeResource {
            environment_id: self.sandbox_id,
            idempotency_key,
            state: map_cube_state(&self.state),
            expires_at: self.end_at.unwrap_or_else(Utc::now),
            tags: self.metadata,
        }
    }
}

fn map_cube_state(value: &str) -> CubeState {
    match value.to_ascii_lowercase().as_str() {
        "running" => CubeState::Ready,
        "creating" | "resuming" => CubeState::Creating,
        "killed" | "terminated" => CubeState::Deleted,
        "failed" => CubeState::Failed,
        _ => CubeState::Unknown,
    }
}

#[derive(Clone, PartialEq, Message)]
struct StartRequest {
    #[prost(message, optional, tag = "1")]
    process: Option<ProcessConfig>,
    #[prost(bool, optional, tag = "4")]
    stdin: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ProcessConfig {
    #[prost(string, tag = "1")]
    cmd: String,
    #[prost(string, repeated, tag = "2")]
    args: Vec<String>,
    #[prost(map = "string, string", tag = "3")]
    envs: HashMap<String, String>,
    #[prost(string, optional, tag = "4")]
    cwd: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct StartResponse {
    #[prost(message, optional, tag = "1")]
    event: Option<ProcessEvent>,
}

#[derive(Clone, PartialEq, Message)]
struct ProcessEvent {
    #[prost(oneof = "process_event::Event", tags = "1, 2, 3, 4")]
    event: Option<process_event::Event>,
}

mod process_event {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Start(super::StartEvent),
        #[prost(message, tag = "2")]
        Data(super::DataEvent),
        #[prost(message, tag = "3")]
        End(super::EndEvent),
        #[prost(message, tag = "4")]
        Keepalive(super::KeepAlive),
    }
}

#[derive(Clone, PartialEq, Message)]
struct StartEvent {
    #[prost(uint32, tag = "1")]
    pid: u32,
}

#[derive(Clone, PartialEq, Message)]
struct DataEvent {
    #[prost(oneof = "data_event::Output", tags = "1, 2, 3")]
    output: Option<data_event::Output>,
}

mod data_event {
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Output {
        #[prost(bytes, tag = "1")]
        Stdout(Vec<u8>),
        #[prost(bytes, tag = "2")]
        Stderr(Vec<u8>),
        #[prost(bytes, tag = "3")]
        Pty(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
struct EndEvent {
    #[prost(sint32, tag = "1")]
    exit_code: i32,
    #[prost(bool, tag = "2")]
    exited: bool,
    #[prost(string, tag = "3")]
    status: String,
    #[prost(string, optional, tag = "4")]
    error: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct KeepAlive {}

fn decode_start_stream(
    bytes: &[u8],
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<(i32, Vec<u8>, Vec<u8>), BackendError> {
    let mut cursor = 0usize;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while cursor < bytes.len() {
        if bytes.len() - cursor < 5 {
            return Err(BackendError::Transport(
                "truncated Cube Connect envelope".into(),
            ));
        }
        let flags = bytes[cursor];
        let length = u32::from_be_bytes(bytes[cursor + 1..cursor + 5].try_into().unwrap()) as usize;
        cursor += 5;
        if length > bytes.len() - cursor {
            return Err(BackendError::Transport(
                "truncated Cube Connect message".into(),
            ));
        }
        if flags == 0x02 {
            let end: serde_json::Value = serde_json::from_slice(&bytes[cursor..cursor + length])
                .map_err(|error| {
                    BackendError::Transport(format!("decode Cube Connect end stream: {error}"))
                })?;
            cursor += length;
            if let Some(error) = end.get("error") {
                return Err(BackendError::Transport(format!(
                    "Cube Connect stream failed: {error}"
                )));
            }
            continue;
        }
        if flags != 0 {
            return Err(BackendError::Transport(format!(
                "unsupported Cube Connect envelope flags {flags}"
            )));
        }
        let response = StartResponse::decode(&bytes[cursor..cursor + length]).map_err(|error| {
            BackendError::Transport(format!("decode Cube command event: {error}"))
        })?;
        cursor += length;
        match response.event.and_then(|event| event.event) {
            Some(process_event::Event::Data(data)) => match data.output {
                Some(data_event::Output::Stdout(value)) => {
                    append_bounded(&mut stdout, value, stdout_limit, "stdout")?
                }
                Some(data_event::Output::Stderr(value)) => {
                    append_bounded(&mut stderr, value, stderr_limit, "stderr")?
                }
                Some(data_event::Output::Pty(_)) | None => {
                    return Err(BackendError::Transport(
                        "unexpected PTY event from Cube command".into(),
                    ));
                }
            },
            Some(process_event::Event::End(end)) => {
                if !end.exited {
                    return Err(BackendError::Unknown(format!(
                        "Cube command did not exit normally: {} {}",
                        end.status,
                        end.error.unwrap_or_default()
                    )));
                }
                exit_code = Some(end.exit_code);
            }
            Some(process_event::Event::Start(_)) | Some(process_event::Event::Keepalive(_)) => {}
            None => return Err(BackendError::Transport("empty Cube command event".into())),
        }
    }
    let exit_code = exit_code.ok_or_else(|| {
        BackendError::Unknown("Cube command stream ended without a terminal event".into())
    })?;
    Ok((exit_code, stdout, stderr))
}

fn append_bounded(
    output: &mut Vec<u8>,
    value: Vec<u8>,
    maximum: usize,
    stream: &str,
) -> Result<(), BackendError> {
    if output.len().saturating_add(value.len()) > maximum {
        return Err(BackendError::Transport(format!(
            "Cube command {stream} exceeded the admitted limit"
        )));
    }
    output.extend_from_slice(&value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn connect_stream_decoder_bounds_output_and_requires_end() {
        let messages = [
            StartResponse {
                event: Some(ProcessEvent {
                    event: Some(process_event::Event::Data(DataEvent {
                        output: Some(data_event::Output::Stdout(b"ok".to_vec())),
                    })),
                }),
            },
            StartResponse {
                event: Some(ProcessEvent {
                    event: Some(process_event::Event::End(EndEvent {
                        exit_code: 0,
                        exited: true,
                        status: "exited".into(),
                        error: None,
                    })),
                }),
            },
        ];
        let mut stream = Vec::new();
        for message in messages {
            let payload = message.encode_to_vec();
            stream.push(0);
            stream.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            stream.extend_from_slice(&payload);
        }
        stream.push(0x02);
        stream.extend_from_slice(&2u32.to_be_bytes());
        stream.extend_from_slice(b"{}");
        let result = decode_start_stream(&stream, 2, 1).unwrap();
        assert_eq!(result, (0, b"ok".to_vec(), Vec::new()));
        assert!(decode_start_stream(&stream, 1, 1).is_err());
    }

    #[test]
    fn refuses_insecure_or_credentialed_base_urls() {
        let config = CubeHttpClientConfig {
            api_url: Url::parse("http://cube.example/api").unwrap(),
            sandbox_url: None,
            api_key: "secret".into(),
            request_timeout: Duration::from_secs(1),
            maximum_response_bytes: 1024,
            allow_insecure_http: false,
            tls_ca_pem: None,
        };
        assert!(CubeHttpClient::new(config).is_err());
    }

    #[tokio::test]
    async fn create_uses_authenticated_deny_all_cube_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(headers_end) = request.windows(4).position(|value| value == b"\r\n\r\n")
                {
                    let headers_end = headers_end + 4;
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_string)
                        })
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= headers_end + length {
                        break;
                    }
                }
            }
            socket
                .write_all(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 22\r\nConnection: close\r\n\r\n{\"sandboxID\":\"cube-1\"}")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        let client = CubeHttpClient::new(CubeHttpClientConfig {
            api_url: Url::parse(&format!("http://{address}/")).unwrap(),
            sandbox_url: None,
            api_key: "test-secret".into(),
            request_timeout: Duration::from_secs(2),
            maximum_response_bytes: 4096,
            allow_insecure_http: true,
            tls_ca_pem: None,
        })
        .unwrap();
        let resource = client
            .create(CubeCreateRequest {
                idempotency_key: "light:one".into(),
                template_id: "template".into(),
                expires_at: Utc::now() + chrono::Duration::seconds(60),
                deny_all_egress: true,
                credentials_enabled: false,
                tags: BTreeMap::from([("light.runner".into(), "runner".into())]),
                inputs: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(resource.environment_id, "cube-1");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /sandboxes HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret")
        );
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["allow_internet_access"], false);
        assert_eq!(body["network"]["allowPublicTraffic"], false);
        assert_eq!(
            body["network"]["denyOut"],
            serde_json::json!(["0.0.0.0/0", "::/0"])
        );
        assert_eq!(body["lifecycle"]["onTimeout"], "kill");
        assert_eq!(body["metadata"]["light.runner"], "runner");
    }
}
