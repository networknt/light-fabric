use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use futures_util::{StreamExt, future::join_all};
use model_provider::conformance::{
    CaseResult, ConformanceResult, Ed25519EvidenceSigner, LiveProbeReport, LiveQualificationSpec,
    build_live_conformance_result,
};
use model_provider::inference::{Operation, ProviderProtocol, StreamDecoder};
use model_provider::providers::{
    anthropic::{AnthropicCodec, AnthropicStreamDecoder},
    openai::{
        OpenAiCodec, OpenAiResponsesCodec, OpenAiResponsesStreamDecoder, OpenAiStreamDecoder,
    },
};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerConfig {
    pub host_id: String,
    pub pending_work_url: String,
    pub completion_url: String,
    pub workload_token_file: PathBuf,
    pub signing_seed_file: PathBuf,
    pub signer_key_id: String,
    pub singleton_lock_file: PathBuf,
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default)]
    pub credential_environment: BTreeMap<String, String>,
    #[serde(default)]
    pub trust_bundle_files: BTreeMap<String, PathBuf>,
    pub runner_vantage: RunnerVantageIdentity,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerVantageIdentity {
    pub kind: String,
    pub environment_id: String,
    pub source_workload_id: String,
    pub source_network_zone_id: String,
    pub source_network_namespace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingQualificationTask {
    pub host_id: String,
    pub deployment_id: String,
    pub aggregate_version: i64,
    pub endpoint_url: String,
    pub model: String,
    pub endpoint_auth: ProbeEndpointAuth,
    #[serde(default)]
    pub trust_bundle_ref: Option<String>,
    pub qualification: LiveQualificationSpec,
    #[serde(default)]
    pub sidecar: Option<SidecarProbeContract>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProbeEndpointAuth {
    None,
    Bearer { credential_ref: String },
    XApiKey { credential_ref: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarProbeContract {
    pub identity_path: String,
    pub health_path: String,
    pub expected_identity: Value,
    pub denied_probes: Vec<MethodPath>,
    pub raw_runtime_probe_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MethodPath {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingWorkPage {
    #[serde(default)]
    pub tasks: Vec<PendingQualificationTask>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualificationCompletion<'a> {
    pub host_id: &'a str,
    pub provider_deployment_id: &'a str,
    pub aggregate_version: i64,
    pub conformance_state: &'static str,
    pub conformance_digest: &'a str,
    pub conformance_valid_until: String,
    pub conformance_result: &'a ConformanceResult,
}

pub struct SingletonLease {
    file: File,
    path: PathBuf,
}

impl SingletonLease {
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "cannot open qualification runner lock at {}",
                    path.display()
                )
            })?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "qualification runner already active for lock {}",
                path.display()
            )
        })?;
        Ok(Self { file, path })
    }
}

impl Drop for SingletonLease {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            tracing::warn!(
                error = %error,
                path = %self.path.display(),
                "qualification runner lock release failed"
            );
        }
    }
}

pub struct QualificationRunner {
    control_client: Client,
    config: RunnerConfig,
    signer: Ed25519EvidenceSigner,
    _lease: SingletonLease,
}

impl QualificationRunner {
    pub fn new(config: RunnerConfig) -> Result<Self> {
        let lease = SingletonLease::acquire(&config.singleton_lock_file)?;
        let seed = read_exact_seed(&config.signing_seed_file)?;
        let signer = Ed25519EvidenceSigner::from_seed(&config.signer_key_id, &seed)
            .map_err(|error| anyhow!(error))?;
        let control_client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            control_client,
            config,
            signer,
            _lease: lease,
        })
    }

    pub async fn run_forever(&self) -> Result<()> {
        loop {
            if let Err(error) = self.run_pending_page().await {
                tracing::error!(error = %error, "qualification page failed");
            }
            tokio::time::sleep(Duration::from_secs(self.config.poll_seconds.max(1))).await;
        }
    }

    pub async fn run_pending_page(&self) -> Result<usize> {
        let token = read_trimmed(&self.config.workload_token_file)?;
        let page = self
            .control_client
            .post(&self.config.pending_work_url)
            .bearer_auth(&token)
            .json(&json!({
                "host": "lightapi.net",
                "service": "genai",
                "action": "getPendingLlmProviderQualificationWork",
                "version": "0.1.0",
                "data": {
                    "hostId": self.config.host_id,
                    "limit": self.config.page_size
                }
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<PendingWorkPage>()
            .await?;
        let count = page.tasks.len();
        for task in page.tasks {
            if let Err(error) = self.process_task(&task).await {
                tracing::error!(
                    deployment_id = %task.deployment_id,
                    aggregate_version = task.aggregate_version,
                    error = %error,
                    "qualification task failed; continuing with remaining page"
                );
            }
        }
        Ok(count)
    }

    async fn process_task(&self, task: &PendingQualificationTask) -> Result<()> {
        let mut task = task.clone();
        if let Some(vantage) = task.qualification.live_evidence.runner_vantage.as_mut() {
            vantage.kind.clone_from(&self.config.runner_vantage.kind);
            vantage
                .environment_id
                .clone_from(&self.config.runner_vantage.environment_id);
            vantage
                .source_workload_id
                .clone_from(&self.config.runner_vantage.source_workload_id);
            vantage
                .source_network_zone_id
                .clone_from(&self.config.runner_vantage.source_network_zone_id);
            vantage
                .source_network_namespace_id
                .clone_from(&self.config.runner_vantage.source_network_namespace_id);
        }
        let materials = QualificationMaterials {
            credential_environment: &self.config.credential_environment,
            trust_bundle_files: &self.config.trust_bundle_files,
        };
        let mut result = qualify_with_materials(&task, &materials).await?;
        result.sign(&self.signer).map_err(|error| anyhow!(error))?;
        let completion = QualificationCompletion {
            host_id: &task.host_id,
            provider_deployment_id: &task.deployment_id,
            aggregate_version: task.aggregate_version,
            conformance_state: match result.state {
                model_provider::conformance::ConformanceState::Pass => "PASS",
                model_provider::conformance::ConformanceState::Fail => "FAIL",
                model_provider::conformance::ConformanceState::Quarantined => "QUARANTINED",
            },
            conformance_digest: &result.digest,
            conformance_valid_until: result.valid_until.to_rfc3339(),
            conformance_result: &result,
        };
        self.control_client
            .post(&self.config.completion_url)
            .bearer_auth(read_trimmed(&self.config.workload_token_file)?)
            .json(&json!({
                "host": "lightapi.net",
                "service": "genai",
                "action": "recordLlmProviderConformanceResult",
                "version": "0.1.0",
                "data": completion
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

pub async fn qualify(task: &PendingQualificationTask) -> Result<ConformanceResult> {
    let credential_environment = BTreeMap::new();
    let trust_bundle_files = BTreeMap::new();
    qualify_with_materials(
        task,
        &QualificationMaterials {
            credential_environment: &credential_environment,
            trust_bundle_files: &trust_bundle_files,
        },
    )
    .await
}

#[derive(Debug)]
struct QualificationMaterials<'a> {
    credential_environment: &'a BTreeMap<String, String>,
    trust_bundle_files: &'a BTreeMap<String, PathBuf>,
}

async fn qualify_with_materials(
    task: &PendingQualificationTask,
    materials: &QualificationMaterials<'_>,
) -> Result<ConformanceResult> {
    validate_task(task)?;
    let client = endpoint_client(task, materials)?;
    let mut report = LiveProbeReport::default();

    // The first request is the cold-start observation and also performs the
    // explicit warmup before any parallel or queue-saturation traffic.
    let cold_started = Instant::now();
    let cold_result = probe_operation(&client, task, materials, false).await;
    report.cold_start_millis =
        u64::try_from(cold_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    report.warmup_succeeded = cold_result.is_ok();
    report.cases.push(
        if cold_result.is_ok()
            && report.cold_start_millis <= task.qualification.cold_start_timeout_ms
        {
            pass("cold_start_warmup")
        } else if cold_result.is_ok() {
            fail("cold_start_warmup", "cold_start_timeout")
        } else {
            fail("cold_start_warmup", "warmup_failed")
        },
    );

    for operation in &task.qualification.operations {
        if *operation != task.qualification.provider.operation() {
            report
                .cases
                .push(fail("operation", "protocol_operation_mismatch"));
            continue;
        }
        match probe_operation(&client, task, materials, false).await {
            Ok(observation) => {
                report.successful_operations.insert(*operation);
                if *operation == Operation::Embed {
                    report.embedding_format_succeeded = observation.embedding_format_succeeded;
                }
                report.cases.push(pass(match operation {
                    Operation::Generate => "generate",
                    Operation::Embed => "embed",
                }));
            }
            Err(_) => report.cases.push(fail(
                match operation {
                    Operation::Generate => "generate",
                    Operation::Embed => "embed",
                },
                "operation_probe_failed",
            )),
        }
    }

    let parallel = (0..task.qualification.max_parallel_requests)
        .map(|_| probe_operation(&client, task, materials, false));
    report.parallelism_observed = join_all(parallel)
        .await
        .into_iter()
        .filter(Result::is_ok)
        .count();
    report.cases.push(
        if report.parallelism_observed >= task.qualification.max_parallel_requests {
            pass("parallelism")
        } else {
            fail("parallelism", "qualified_parallelism_not_met")
        },
    );

    let saturation_count = task
        .qualification
        .max_parallel_requests
        .saturating_add(task.qualification.max_queued_requests);
    let saturation =
        (0..saturation_count).map(|_| probe_operation(&client, task, materials, false));
    report.queue_saturation_observed = join_all(saturation)
        .await
        .into_iter()
        .filter(Result::is_ok)
        .count();
    report
        .cases
        .push(if report.queue_saturation_observed >= saturation_count {
            pass("queue_saturation")
        } else {
            fail("queue_saturation", "qualified_queue_capacity_not_met")
        });

    if task.qualification.operations.contains(&Operation::Generate) {
        report.streaming_succeeded = probe_operation(&client, task, materials, true)
            .await
            .is_ok();
        report.disconnect_succeeded = probe_disconnect_and_recover(&client, task, materials)
            .await
            .is_ok();
        report.cases.push(if report.streaming_succeeded {
            pass("streaming")
        } else {
            fail("streaming", "stream_probe_failed")
        });
        report.cases.push(if report.disconnect_succeeded {
            pass("streaming_disconnect")
        } else {
            fail("streaming_disconnect", "disconnect_recovery_failed")
        });
    }

    report.timeout_bounds_succeeded = report.warmup_succeeded
        && report.cold_start_millis <= task.qualification.cold_start_timeout_ms
        && report.parallelism_observed >= task.qualification.max_parallel_requests
        && report.queue_saturation_observed >= saturation_count;
    report.cases.push(if report.timeout_bounds_succeeded {
        pass("timeout_bounds")
    } else {
        fail("timeout_bounds", "declared_timeout_exceeded")
    });

    if let Some(sidecar) = &task.sidecar {
        probe_sidecar(&client, task, materials, sidecar, &mut report).await;
    }

    build_live_conformance_result(&task.qualification, report).map_err(|error| anyhow!(error))
}

fn validate_task(task: &PendingQualificationTask) -> Result<()> {
    let endpoint = reqwest::Url::parse(&task.endpoint_url)?;
    if endpoint.query().is_some() || endpoint.fragment().is_some() {
        bail!("qualified endpoint cannot contain query or fragment");
    }
    if task.qualification.operations.is_empty() {
        bail!("qualification task has no operations");
    }
    if task.qualification.max_parallel_requests > 256
        || task.qualification.max_queued_requests > 1_024
    {
        bail!("qualification task exceeds bounded concurrency limits");
    }
    if let Some(sidecar) = &task.sidecar {
        validate_raw_probe_target(sidecar)?;
    }
    Ok(())
}

fn validate_raw_probe_target(sidecar: &SidecarProbeContract) -> Result<()> {
    let url = reqwest::Url::parse(&sidecar.raw_runtime_probe_url)
        .context("raw runtime probe URL is invalid")?;
    if url.scheme() != "http"
        || url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_none()
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("raw runtime probe must be a fixed HTTP IP address with an explicit port");
    }
    Ok(())
}

fn endpoint_client(
    task: &PendingQualificationTask,
    materials: &QualificationMaterials<'_>,
) -> Result<Client> {
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(task.qualification.request_timeout_ms));
    if let Some(reference) = &task.trust_bundle_ref {
        let path = materials
            .trust_bundle_files
            .get(reference)
            .ok_or_else(|| anyhow!("trust-bundle reference is not in the runner allowlist"))?;
        let certificates = reqwest::Certificate::from_pem_bundle(&fs::read(path)?)?;
        if certificates.is_empty() {
            bail!("qualification trust bundle is empty");
        }
        builder = builder.tls_built_in_root_certs(false);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

async fn probe_operation(
    client: &Client,
    task: &PendingQualificationTask,
    materials: &QualificationMaterials<'_>,
    stream: bool,
) -> Result<ProbeObservation> {
    let (url, body) = operation_request(task, stream)?;
    let request =
        apply_endpoint_auth(client.post(url).json(&body), &task.endpoint_auth, materials)?;
    let response = tokio::time::timeout(
        Duration::from_millis(task.qualification.request_timeout_ms),
        request.send(),
    )
    .await
    .map_err(|_| anyhow!("operation request timeout"))??;
    if response.status() != StatusCode::OK {
        bail!("probe status rejected");
    }
    if stream {
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            bail!("stream content type rejected");
        }
        let mut chunks = response.bytes_stream();
        let mut decoder = QualificationStreamDecoder::new(task.qualification.provider)?;
        let setup_deadline = tokio::time::Instant::now()
            + Duration::from_millis(task.qualification.stream_setup_timeout_ms);
        let mut event_count = 0_usize;
        while event_count == 0 {
            let chunk = tokio::time::timeout_at(setup_deadline, chunks.next())
                .await
                .map_err(|_| anyhow!("stream setup timeout"))?
                .ok_or_else(|| anyhow!("stream ended before first decoded event"))??;
            event_count += decoder.push(&chunk)?.len();
        }
        while let Some(chunk) = chunks.next().await {
            event_count += decoder.push(&chunk?)?.len();
        }
        event_count += decoder.finish()?.len();
        if event_count == 0 {
            bail!("stream contained no provider-codec events");
        }
        Ok(ProbeObservation::default())
    } else {
        let bytes = response.bytes().await?;
        let value = serde_json::from_slice::<Value>(&bytes)?;
        let embedding_format_succeeded =
            if task.qualification.provider.operation() == Operation::Embed {
                validate_embedding_response(task, &value, bytes.len())?;
                true
            } else {
                validate_generation_response(task.qualification.provider, &value)?;
                false
            };
        Ok(ProbeObservation {
            embedding_format_succeeded,
        })
    }
}

fn validate_generation_response(protocol: ProviderProtocol, value: &Value) -> Result<()> {
    match protocol {
        ProviderProtocol::OpenAiChat => OpenAiCodec::default()
            .decode_response(value)
            .map(|_| ())
            .map_err(|error| anyhow!(error)),
        ProviderProtocol::OpenAiResponses => OpenAiResponsesCodec::default()
            .decode_response(value)
            .map(|_| ())
            .map_err(|error| anyhow!(error)),
        ProviderProtocol::AnthropicMessages => AnthropicCodec::default()
            .decode_response(value)
            .map(|_| ())
            .map_err(|error| anyhow!(error)),
        ProviderProtocol::OpenAiEmbeddings => bail!("embedding protocol used for generation"),
    }
}

enum QualificationStreamDecoder {
    OpenAi(OpenAiStreamDecoder),
    Responses(OpenAiResponsesStreamDecoder),
    Anthropic(AnthropicStreamDecoder),
}

impl QualificationStreamDecoder {
    fn new(protocol: ProviderProtocol) -> Result<Self> {
        match protocol {
            ProviderProtocol::OpenAiChat => Ok(Self::OpenAi(OpenAiStreamDecoder::default())),
            ProviderProtocol::OpenAiResponses => {
                Ok(Self::Responses(OpenAiResponsesStreamDecoder::default()))
            }
            ProviderProtocol::AnthropicMessages => {
                Ok(Self::Anthropic(AnthropicStreamDecoder::default()))
            }
            ProviderProtocol::OpenAiEmbeddings => bail!("embedding protocol cannot stream"),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<model_provider::inference::InferenceEvent>> {
        match self {
            Self::OpenAi(decoder) => decoder.push(chunk),
            Self::Responses(decoder) => decoder.push(chunk),
            Self::Anthropic(decoder) => decoder.push(chunk),
        }
        .map_err(|error| anyhow!(error))
    }

    fn finish(&mut self) -> Result<Vec<model_provider::inference::InferenceEvent>> {
        match self {
            Self::OpenAi(decoder) => decoder.finish(),
            Self::Responses(decoder) => decoder.finish(),
            Self::Anthropic(decoder) => decoder.finish(),
        }
        .map_err(|error| anyhow!(error))
    }
}

#[derive(Debug, Default)]
struct ProbeObservation {
    embedding_format_succeeded: bool,
}

fn operation_request(
    task: &PendingQualificationTask,
    stream: bool,
) -> Result<(reqwest::Url, Value)> {
    let (path, body) = match task.qualification.provider {
        ProviderProtocol::OpenAiChat => (
            "/v1/chat/completions",
            json!({"model": task.model, "messages": [{"role": "user", "content": "qualification"}], "max_tokens": 1, "stream": stream}),
        ),
        ProviderProtocol::OpenAiResponses => (
            "/v1/responses",
            json!({"model": task.model, "input": "qualification", "max_output_tokens": 1, "stream": stream}),
        ),
        ProviderProtocol::OpenAiEmbeddings => (
            "/v1/embeddings",
            json!({"model": task.model, "input": ["qualification"], "encoding_format": "float"}),
        ),
        ProviderProtocol::AnthropicMessages => (
            "/v1/messages",
            json!({"model": task.model, "messages": [{"role": "user", "content": "qualification"}], "max_tokens": 1, "stream": stream}),
        ),
    };
    Ok((join_endpoint(&task.endpoint_url, path)?, body))
}

async fn probe_disconnect_and_recover(
    client: &Client,
    task: &PendingQualificationTask,
    materials: &QualificationMaterials<'_>,
) -> Result<()> {
    let (url, body) = operation_request(task, true)?;
    let response =
        apply_endpoint_auth(client.post(url).json(&body), &task.endpoint_auth, materials)?
            .send()
            .await?
            .error_for_status()?;
    let mut chunks = response.bytes_stream();
    let first = tokio::time::timeout(
        Duration::from_millis(task.qualification.stream_setup_timeout_ms),
        chunks.next(),
    )
    .await
    .map_err(|_| anyhow!("disconnect probe stream setup timeout"))?
    .ok_or_else(|| anyhow!("disconnect probe ended before first frame"))??;
    if first.is_empty() {
        bail!("disconnect probe first frame is empty");
    }
    drop(chunks);
    // A fresh bounded request must succeed after consumer cancellation. This
    // proves the cancelled stream released its client/pool capacity.
    probe_operation(client, task, materials, false).await?;
    Ok(())
}

fn validate_embedding_response(
    task: &PendingQualificationTask,
    value: &Value,
    bytes: usize,
) -> Result<()> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("embedding response has no data array"))?;
    if data.len() != 1 {
        bail!("embedding response item count mismatch");
    }
    let vector = data[0]
        .get("embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("embedding response is not float encoded"))?;
    if vector.is_empty()
        || vector
            .iter()
            .any(|component| component.as_f64().is_none_or(|number| !number.is_finite()))
    {
        bail!("embedding response contains invalid vector components");
    }
    if let Some(capabilities) = &task.qualification.embedding_capabilities {
        if capabilities.max_response_bytes > 0 && bytes > capabilities.max_response_bytes {
            bail!("embedding response exceeds the declared byte limit");
        }
        let dimension =
            u32::try_from(vector.len()).map_err(|_| anyhow!("embedding dimension overflow"))?;
        if let Some(space) = &capabilities.space
            && dimension != space.dimension
        {
            bail!("embedding response does not match the declared space dimension");
        }
        if !capabilities.supported_dimensions.is_empty()
            && !capabilities.supported_dimensions.contains(&dimension)
        {
            bail!("embedding response dimension is not declared");
        }
        if !capabilities.supported_encodings.is_empty()
            && !capabilities
                .supported_encodings
                .contains(&model_provider::inference::capabilities::EmbeddingEncoding::Float)
        {
            bail!("float embedding encoding is not declared");
        }
    }
    Ok(())
}

async fn probe_sidecar(
    client: &Client,
    task: &PendingQualificationTask,
    materials: &QualificationMaterials<'_>,
    sidecar: &SidecarProbeContract,
    report: &mut LiveProbeReport,
) {
    let identity_ok = async {
        let response = apply_endpoint_auth(
            client.get(join_endpoint(&task.endpoint_url, &sidecar.identity_path)?),
            &task.endpoint_auth,
            materials,
        )?
        .send()
        .await?
        .error_for_status()?;
        Ok::<_, anyhow::Error>(response.json::<Value>().await? == sidecar.expected_identity)
    }
    .await
    .unwrap_or(false);
    report.cases.push(if identity_ok {
        pass("sidecar_identity")
    } else {
        fail("sidecar_identity", "sidecar_identity_mismatch")
    });

    let health_ok = match join_endpoint(&task.endpoint_url, &sidecar.health_path) {
        Ok(url) => client
            .get(url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success()),
        Err(_) => false,
    };
    report.cases.push(if health_ok {
        pass("sidecar_health")
    } else {
        fail("sidecar_health", "sidecar_health_failed")
    });

    for denied in &sidecar.denied_probes {
        let denied_ok = match (
            denied.method.parse(),
            join_endpoint(&task.endpoint_url, &denied.path),
        ) {
            (Ok(method), Ok(url)) => client
                .request(method, url)
                .send()
                .await
                .is_ok_and(|response| response.status() == StatusCode::NOT_FOUND),
            _ => false,
        };
        report.cases.push(if denied_ok {
            pass("sidecar_deny")
        } else {
            fail("sidecar_deny", "sidecar_path_exposed")
        });
    }

    let expected_target_digest = task
        .qualification
        .live_evidence
        .runner_vantage
        .as_ref()
        .map(|vantage| vantage.raw_probe_target_sha256.as_str());
    let actual_target_digest = format!(
        "{:x}",
        Sha256::digest(sidecar.raw_runtime_probe_url.as_bytes())
    );
    let raw_unreachable = expected_target_digest == Some(actual_target_digest.as_str())
        && match client.get(&sidecar.raw_runtime_probe_url).send().await {
            Ok(_) => false,
            Err(error) => error.is_connect() || error.is_timeout(),
        };
    report.cases.push(if raw_unreachable {
        pass("raw_runtime_unreachable")
    } else {
        fail("raw_runtime_unreachable", "raw_runtime_reachable")
    });
}

fn apply_endpoint_auth(
    request: reqwest::RequestBuilder,
    auth: &ProbeEndpointAuth,
    materials: &QualificationMaterials<'_>,
) -> Result<reqwest::RequestBuilder> {
    match auth {
        ProbeEndpointAuth::None => Ok(request),
        ProbeEndpointAuth::Bearer { credential_ref } => {
            Ok(request.bearer_auth(resolve_credential(credential_ref, materials)?))
        }
        ProbeEndpointAuth::XApiKey { credential_ref } => {
            Ok(request.header("x-api-key", resolve_credential(credential_ref, materials)?))
        }
    }
}

fn join_endpoint(base: &str, path: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn resolve_credential(reference: &str, materials: &QualificationMaterials<'_>) -> Result<String> {
    let environment = materials
        .credential_environment
        .get(reference)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("credential reference is not in the runner allowlist"))?;
    let value = std::env::var(environment)
        .with_context(|| "protected credential binding is absent".to_string())?;
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("protected credential binding resolved to an empty value");
    }
    Ok(value)
}

fn read_exact_seed(path: &Path) -> Result<[u8; 32]> {
    let value = fs::read(path)?;
    value
        .try_into()
        .map_err(|_| anyhow!("Ed25519 seed file must contain exactly 32 bytes"))
}

fn read_trimmed(path: &Path) -> Result<String> {
    let value = fs::read_to_string(path)?;
    let value = value.trim();
    if value.is_empty() {
        bail!("protected token file is empty");
    }
    Ok(value.to_string())
}

fn pass(id: &'static str) -> CaseResult {
    CaseResult {
        id: id.to_string(),
        passed: true,
        detail: None,
    }
}

fn fail(id: &'static str, category: &'static str) -> CaseResult {
    CaseResult {
        id: id.to_string(),
        passed: false,
        detail: Some(category.to_string()),
    }
}

const fn default_page_size() -> usize {
    20
}

const fn default_poll_seconds() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::{Body, Bytes},
        response::{IntoResponse, Response},
        routing::post,
    };
    use model_provider::conformance::{LiveEvidenceBinding, RunnerVantage};
    use std::collections::BTreeSet;
    use std::convert::Infallible;
    use tokio::net::TcpListener;

    async fn embeddings() -> Json<Value> {
        Json(json!({"data":[{"embedding":[0.1,0.2],"index":0}],"model":"fake"}))
    }

    async fn malformed_embeddings() -> Json<Value> {
        Json(json!({"data":[{"embedding":[0.1,"not-a-number"],"index":0}],"model":"fake"}))
    }

    async fn malformed_generation() -> Json<Value> {
        Json(json!({}))
    }

    async fn chat(Json(request): Json<Value>) -> Response {
        if request.get("stream").and_then(Value::as_bool) == Some(true) {
            let stream = futures_util::stream::unfold(0_u8, |state| async move {
                match state {
                    0 => Some((
                        Ok::<_, Infallible>(Bytes::from_static(
                            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                        )),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Some((
                            Ok::<_, Infallible>(Bytes::from_static(b"data: [DONE]\n\n")),
                            2,
                        ))
                    }
                    _ => None,
                }
            });
            axum::http::Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        } else {
            Json(json!({
                "id":"qualification",
                "choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }))
            .into_response()
        }
    }

    #[tokio::test]
    async fn qualifies_exact_embedding_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/embeddings", post(embeddings)),
            )
            .await
            .unwrap();
        });
        let task = PendingQualificationTask {
            host_id: "host".into(),
            deployment_id: "deployment".into(),
            aggregate_version: 7,
            endpoint_url: format!("http://{address}"),
            model: "fake".into(),
            endpoint_auth: ProbeEndpointAuth::None,
            trust_bundle_ref: None,
            qualification: LiveQualificationSpec {
                provider: ProviderProtocol::OpenAiEmbeddings,
                provider_codec_version: "openai-v1".into(),
                physical_model: "fake".into(),
                api_version: "v1".into(),
                operations: BTreeSet::from([Operation::Embed]),
                live_evidence: LiveEvidenceBinding {
                    deployment_revision_id: "deployment/r7".into(),
                    provider_endpoint_sha256: "a".repeat(64),
                    network_profile_sha256: "b".repeat(64),
                    physical_runtime_id: "runtime".into(),
                    capacity_declaration_sha256: "c".repeat(64),
                    sidecar: None,
                    runner_vantage: Some(RunnerVantage {
                        kind: "standalone".into(),
                        environment_id: "test".into(),
                        source_workload_id: "runner".into(),
                        source_network_zone_id: "runner".into(),
                        source_network_namespace_id: "runner".into(),
                        target_network_namespace_id: "runtime".into(),
                        raw_probe_target_sha256: "d".repeat(64),
                    }),
                },
                valid_for_seconds: 3600,
                refresh_before_seconds: 300,
                max_parallel_requests: 2,
                max_queued_requests: 2,
                cold_start_timeout_ms: 5_000,
                stream_setup_timeout_ms: 1_000,
                request_timeout_ms: 5_000,
                embedding_capabilities: None,
            },
            sidecar: None,
        };
        let result = qualify(&task).await.unwrap();
        assert_eq!(
            result.state,
            model_provider::conformance::ConformanceState::Pass
        );
        assert_eq!(
            result.evidence_kind,
            model_provider::conformance::EvidenceKind::LiveEndpoint
        );
        assert!(
            result
                .cases
                .iter()
                .any(|case| case.id == "queue_saturation" && case.passed)
        );
        assert!(
            result
                .cases
                .iter()
                .any(|case| case.id == "timeout_bounds" && case.passed)
        );
    }

    #[tokio::test]
    async fn malformed_embedding_payload_cannot_produce_pass_evidence() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/embeddings", post(malformed_embeddings)),
            )
            .await
            .unwrap();
        });
        let mut task = embedding_task(format!("http://{address}"));
        task.qualification.max_parallel_requests = 1;
        task.qualification.max_queued_requests = 1;
        let result = qualify(&task).await.unwrap();
        assert_eq!(
            result.state,
            model_provider::conformance::ConformanceState::Fail
        );
        assert!(!result.tested_operations.contains(&Operation::Embed));
    }

    #[tokio::test]
    async fn generation_qualification_proves_streaming_and_disconnect_recovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/chat/completions", post(chat)),
            )
            .await
            .unwrap();
        });
        let task = generation_task(format!("http://{address}"));
        let result = qualify(&task).await.unwrap();
        assert_eq!(
            result.state,
            model_provider::conformance::ConformanceState::Pass
        );
        assert!(result.capabilities.generation.unwrap().streaming);
        assert!(
            result
                .cases
                .iter()
                .any(|case| case.id == "streaming_disconnect" && case.passed)
        );
    }

    #[tokio::test]
    async fn arbitrary_json_cannot_produce_generation_pass_evidence() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/chat/completions", post(malformed_generation)),
            )
            .await
            .unwrap();
        });
        let mut task = generation_task(format!("http://{address}"));
        task.qualification.max_parallel_requests = 1;
        task.qualification.max_queued_requests = 1;
        let result = qualify(&task).await.unwrap();
        assert_eq!(
            result.state,
            model_provider::conformance::ConformanceState::Fail
        );
        assert!(!result.tested_operations.contains(&Operation::Generate));
    }

    #[test]
    fn singleton_lock_is_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runner.lock");
        let first = SingletonLease::acquire(&path).unwrap();
        assert!(SingletonLease::acquire(&path).is_err());
        drop(first);
        assert!(SingletonLease::acquire(path).is_ok());
    }

    fn embedding_task(endpoint_url: String) -> PendingQualificationTask {
        PendingQualificationTask {
            host_id: "host".into(),
            deployment_id: "deployment".into(),
            aggregate_version: 7,
            endpoint_url,
            model: "fake".into(),
            endpoint_auth: ProbeEndpointAuth::None,
            trust_bundle_ref: None,
            qualification: LiveQualificationSpec {
                provider: ProviderProtocol::OpenAiEmbeddings,
                provider_codec_version: "openai-v1".into(),
                physical_model: "fake".into(),
                api_version: "v1".into(),
                operations: BTreeSet::from([Operation::Embed]),
                live_evidence: LiveEvidenceBinding {
                    deployment_revision_id: "deployment/r7".into(),
                    provider_endpoint_sha256: "a".repeat(64),
                    network_profile_sha256: "b".repeat(64),
                    physical_runtime_id: "runtime".into(),
                    capacity_declaration_sha256: "c".repeat(64),
                    sidecar: None,
                    runner_vantage: Some(RunnerVantage {
                        kind: "standalone".into(),
                        environment_id: "test".into(),
                        source_workload_id: "runner".into(),
                        source_network_zone_id: "runner".into(),
                        source_network_namespace_id: "runner".into(),
                        target_network_namespace_id: "runtime".into(),
                        raw_probe_target_sha256: "d".repeat(64),
                    }),
                },
                valid_for_seconds: 3600,
                refresh_before_seconds: 300,
                max_parallel_requests: 2,
                max_queued_requests: 2,
                cold_start_timeout_ms: 5_000,
                stream_setup_timeout_ms: 1_000,
                request_timeout_ms: 5_000,
                embedding_capabilities: None,
            },
            sidecar: None,
        }
    }

    fn generation_task(endpoint_url: String) -> PendingQualificationTask {
        let mut task = embedding_task(endpoint_url);
        task.qualification.provider = ProviderProtocol::OpenAiChat;
        task.qualification.operations = BTreeSet::from([Operation::Generate]);
        task.qualification.embedding_capabilities = None;
        task
    }
}
