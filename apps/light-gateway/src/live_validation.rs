use anyhow::{Context, Result, bail};
use chrono::Utc;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

const SCHEMA_VERSION: &str = "lightapi.llm.live-validation/v1";
const FIXED_EMBEDDING_TEXT: &str = "light-gateway configuration validation";
const FIXED_GENERATION_TEXT: &str = "Reply with OK.";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LiveOperation {
    Embeddings,
    ChatCompletions,
    Responses,
}

impl LiveOperation {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "embeddings" => Ok(Self::Embeddings),
            "chat-completions" => Ok(Self::ChatCompletions),
            "responses" => Ok(Self::Responses),
            _ => bail!("operation must be embeddings, chat-completions, or responses"),
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Embeddings => "/v1/embeddings",
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveValidationOptions {
    gateway_url: String,
    alias: String,
    operation: LiveOperation,
    header_file: PathBuf,
    ca_file: Option<PathBuf>,
    timeout_seconds: u64,
    output: OutputFormat,
    expected_space_id: Option<String>,
    expected_space_revision: Option<u64>,
    expected_dimension: Option<usize>,
}

pub fn parse_live_validation_options(args: &[String]) -> Result<LiveValidationOptions> {
    let mut gateway_url = None;
    let mut alias = None;
    let mut operation = None;
    let mut header_file = None;
    let mut ca_file = None;
    let mut timeout_seconds = None;
    let mut output = None;
    let mut expected_space_id = None;
    let mut expected_space_revision = None;
    let mut expected_dimension = None;

    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        let value = args
            .get(index + 1)
            .with_context(|| format!("missing value for {option}"))?;
        match option {
            "--gateway-url" => set_once(&mut gateway_url, value, option)?,
            "--alias" => set_once(&mut alias, value, option)?,
            "--operation" => set_once(&mut operation, &LiveOperation::parse(value)?, option)?,
            "--header-file" => set_once(&mut header_file, &PathBuf::from(value), option)?,
            "--ca-file" => set_once(&mut ca_file, &PathBuf::from(value), option)?,
            "--timeout-seconds" => set_once(
                &mut timeout_seconds,
                &value.parse::<u64>().context("timeout must be an integer")?,
                option,
            )?,
            "--output" => {
                let format = match value.as_str() {
                    "json" => OutputFormat::Json,
                    "text" => OutputFormat::Text,
                    _ => bail!("output must be json or text"),
                };
                set_once(&mut output, &format, option)?;
            }
            "--expected-space-id" => set_once(&mut expected_space_id, value, option)?,
            "--expected-space-revision" => set_once(
                &mut expected_space_revision,
                &value
                    .parse::<u64>()
                    .context("embedding-space revision must be an integer")?,
                option,
            )?,
            "--expected-dimension" => set_once(
                &mut expected_dimension,
                &value
                    .parse::<usize>()
                    .context("embedding dimension must be an integer")?,
                option,
            )?,
            _ => bail!("unknown validate-llm-live option `{option}`"),
        }
        index += 2;
    }

    let gateway_url = gateway_url.context("--gateway-url is required")?;
    validate_gateway_url(&gateway_url)?;
    let alias = alias.context("--alias is required")?;
    if !valid_alias(&alias) {
        bail!("Alias must contain only letters, digits, dot, underscore, or hyphen");
    }
    let operation = operation.context("--operation is required")?;
    let header_file = header_file.context("--header-file is required")?;
    let timeout_seconds = timeout_seconds.context("--timeout-seconds is required")?;
    if !(1..=300).contains(&timeout_seconds) {
        bail!("timeout must be between 1 and 300 seconds");
    }
    if expected_space_id
        .as_deref()
        .is_some_and(|value| !valid_embedding_space_id(value))
    {
        bail!("embedding-space ID must be 1-255 visible ASCII characters");
    }

    let embedding_contract_count = usize::from(expected_space_id.is_some())
        + usize::from(expected_space_revision.is_some())
        + usize::from(expected_dimension.is_some());
    match operation {
        LiveOperation::Embeddings if embedding_contract_count != 3 => bail!(
            "embeddings requires --expected-space-id, --expected-space-revision, and --expected-dimension"
        ),
        LiveOperation::Embeddings
            if expected_space_revision == Some(0) || expected_dimension == Some(0) =>
        {
            bail!("embedding-space revision and dimension must be positive")
        }
        LiveOperation::ChatCompletions | LiveOperation::Responses
            if embedding_contract_count != 0 =>
        {
            bail!("embedding-space options are valid only for embeddings")
        }
        _ => {}
    }

    Ok(LiveValidationOptions {
        gateway_url,
        alias,
        operation,
        header_file,
        ca_file,
        timeout_seconds,
        output: output.unwrap_or(OutputFormat::Json),
        expected_space_id,
        expected_space_revision,
        expected_dimension,
    })
}

fn set_once<T: Clone>(target: &mut Option<T>, value: &T, option: &str) -> Result<()> {
    if target.replace(value.clone()).is_some() {
        bail!("duplicate option {option}");
    }
    Ok(())
}

pub fn live_validation_usage() -> &'static str {
    "validate-llm-live --gateway-url URL --operation embeddings|chat-completions|responses \
--alias NAME --header-file PATH [--ca-file PATH] --timeout-seconds N [--output json|text] \
[--expected-space-id ID --expected-space-revision N --expected-dimension N]"
}

fn valid_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_embedding_space_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn validate_gateway_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("gateway URL is invalid")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        bail!("gateway URL must be an HTTPS origin without credentials, path, query, or fragment");
    }
    Ok(url)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveValidationReport {
    schema_version: &'static str,
    status: &'static str,
    category: &'static str,
    operation: LiveOperation,
    alias: String,
    timestamp: String,
    request_id: Option<String>,
    http_status: u16,
    config_generation: Option<u64>,
    billed_cost_micros: Option<u64>,
    contract: Option<EmbeddingContractReport>,
    result: Option<SafeResultSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddingContractReport {
    expected: EmbeddingContract,
    actual: Option<EmbeddingContract>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddingContract {
    space_id: String,
    space_revision: u64,
    dimension: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafeResultSummary {
    object: String,
    item_count: usize,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

pub struct LiveValidationOutcome {
    report: LiveValidationReport,
    output: OutputFormat,
    pub exit_code: i32,
}

impl LiveValidationOutcome {
    pub fn render(&self) -> Result<String> {
        match self.output {
            OutputFormat::Json => Ok(serde_json::to_string_pretty(&self.report)?),
            OutputFormat::Text => Ok(render_text(&self.report)),
        }
    }
}

pub async fn validate_live(options: LiveValidationOptions) -> LiveValidationOutcome {
    let output = options.output;
    match validate_live_inner(&options).await {
        Ok(report) => LiveValidationOutcome {
            report,
            output,
            exit_code: 0,
        },
        Err(failure) => LiveValidationOutcome {
            report: failure.report(&options),
            output,
            exit_code: failure.exit_code,
        },
    }
}

#[derive(Debug)]
struct ValidationFailure {
    category: &'static str,
    exit_code: i32,
    request_id: Option<String>,
    http_status: u16,
}

impl ValidationFailure {
    fn local(category: &'static str, exit_code: i32) -> Self {
        Self {
            category,
            exit_code,
            request_id: None,
            http_status: 0,
        }
    }

    fn report(&self, options: &LiveValidationOptions) -> LiveValidationReport {
        LiveValidationReport {
            schema_version: SCHEMA_VERSION,
            status: "fail",
            category: self.category,
            operation: options.operation,
            alias: options.alias.clone(),
            timestamp: timestamp(),
            request_id: self.request_id.clone(),
            http_status: self.http_status,
            config_generation: None,
            billed_cost_micros: None,
            contract: expected_contract(options).map(|expected| EmbeddingContractReport {
                expected,
                actual: None,
            }),
            result: None,
        }
    }
}

async fn validate_live_inner(
    options: &LiveValidationOptions,
) -> std::result::Result<LiveValidationReport, ValidationFailure> {
    let authorization = read_authorization(&options.header_file)
        .map_err(|_| ValidationFailure::local("protectedHeaderFile", 12))?;
    let client = build_client(options)
        .map_err(|_| ValidationFailure::local("tlsOrClientConfiguration", 12))?;
    let base = validate_gateway_url(&options.gateway_url)
        .map_err(|_| ValidationFailure::local("usage", 10))?;
    let endpoint = base
        .join(options.operation.path())
        .map_err(|_| ValidationFailure::local("usage", 10))?;
    let request_body = fixed_request(options.operation, &options.alias);

    let mut request = client
        .post(endpoint)
        .header(AUTHORIZATION, authorization)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(space_id) = &options.expected_space_id {
        request = request.header("x-light-expected-embedding-space-id", space_id);
    }
    if let Some(revision) = options.expected_space_revision {
        request = request.header(
            "x-light-expected-embedding-space-revision",
            revision.to_string(),
        );
    }

    let response = request
        .body(request_body)
        .send()
        .await
        .map_err(|_| ValidationFailure::local("transport", 13))?;
    let status = response.status();
    let headers = response.headers().clone();
    let request_id = header_string(&headers, "x-request-id");
    let body = read_bounded(response)
        .await
        .map_err(|category| ValidationFailure {
            category,
            exit_code: bounded_read_exit_code(category),
            request_id: request_id.clone(),
            http_status: status.as_u16(),
        })?;

    if !status.is_success() {
        let code = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/code")
                    .or_else(|| value.pointer("/error/type"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let (category, exit_code) =
            failure_category(options.operation, status.as_u16(), code.as_deref());
        return Err(ValidationFailure {
            category,
            exit_code,
            request_id,
            http_status: status.as_u16(),
        });
    }

    let value: Value = serde_json::from_slice(&body).map_err(|_| ValidationFailure {
        category: operation_contract_category(options.operation),
        exit_code: 25,
        request_id: request_id.clone(),
        http_status: status.as_u16(),
    })?;
    let (contract, result) =
        validate_success(options, &headers, &value).map_err(|_| ValidationFailure {
            category: operation_contract_category(options.operation),
            exit_code: 25,
            request_id: request_id.clone(),
            http_status: status.as_u16(),
        })?;
    if request_id.as_deref().is_none_or(str::is_empty) {
        return Err(ValidationFailure {
            category: operation_contract_category(options.operation),
            exit_code: 25,
            request_id,
            http_status: status.as_u16(),
        });
    }

    Ok(LiveValidationReport {
        schema_version: SCHEMA_VERSION,
        status: "pass",
        category: "validated",
        operation: options.operation,
        alias: options.alias.clone(),
        timestamp: timestamp(),
        request_id,
        http_status: status.as_u16(),
        config_generation: header_u64(&headers, "x-light-config-generation"),
        billed_cost_micros: header_u64(&headers, "x-light-billed-cost-micros"),
        contract,
        result: Some(result),
    })
}

fn fixed_request(operation: LiveOperation, alias: &str) -> Vec<u8> {
    match operation {
        LiveOperation::Embeddings => serde_json::to_vec(&json!({
            "model": alias,
            "input": FIXED_EMBEDDING_TEXT
        })),
        LiveOperation::ChatCompletions => serde_json::to_vec(&json!({
            "model": alias,
            "messages": [{"role": "user", "content": FIXED_GENERATION_TEXT}],
            "max_tokens": 1,
            "stream": false
        })),
        LiveOperation::Responses => serde_json::to_vec(&json!({
            "model": alias,
            "input": FIXED_GENERATION_TEXT,
            "max_output_tokens": 1,
            "stream": false
        })),
    }
    .expect("fixed validation request is serializable")
}

fn validate_success(
    options: &LiveValidationOptions,
    headers: &HeaderMap,
    value: &Value,
) -> Result<(Option<EmbeddingContractReport>, SafeResultSummary)> {
    match options.operation {
        LiveOperation::Embeddings => {
            let expected = expected_contract(options).context("expected contract")?;
            let vectors = value
                .get("data")
                .and_then(Value::as_array)
                .context("embedding data")?;
            if value.get("object").and_then(Value::as_str) != Some("list")
                || value.get("model").and_then(Value::as_str) != Some(options.alias.as_str())
                || vectors.len() != 1
            {
                bail!("invalid embedding response shape");
            }
            let dimension = vectors[0]
                .get("embedding")
                .and_then(Value::as_array)
                .map(Vec::len)
                .context("embedding vector")?;
            let actual = EmbeddingContract {
                space_id: header_string(headers, "x-light-embedding-space-id")
                    .context("embedding space ID")?,
                space_revision: header_u64(headers, "x-light-embedding-space-revision")
                    .filter(|value| *value > 0)
                    .context("embedding space revision")?,
                dimension,
            };
            if actual.space_id != expected.space_id
                || actual.space_revision != expected.space_revision
                || actual.dimension != expected.dimension
                || header_u64(headers, "x-light-config-generation").is_none_or(|value| value == 0)
                || header_u64(headers, "x-light-billed-cost-micros").is_none()
            {
                bail!("embedding contract mismatch");
            }
            Ok((
                Some(EmbeddingContractReport {
                    expected,
                    actual: Some(actual),
                }),
                SafeResultSummary {
                    object: "list".to_string(),
                    item_count: vectors.len(),
                    input_tokens: value
                        .pointer("/usage/prompt_tokens")
                        .and_then(Value::as_u64),
                    output_tokens: None,
                },
            ))
        }
        LiveOperation::ChatCompletions => {
            let choices = value
                .get("choices")
                .and_then(Value::as_array)
                .context("chat choices")?;
            if value.get("object").and_then(Value::as_str) != Some("chat.completion")
                || value.get("model").and_then(Value::as_str) != Some(options.alias.as_str())
                || choices.len() != 1
                || !choices[0].get("message").is_some_and(Value::is_object)
            {
                bail!("invalid chat response shape");
            }
            Ok((
                None,
                SafeResultSummary {
                    object: "chat.completion".to_string(),
                    item_count: choices.len(),
                    input_tokens: value
                        .pointer("/usage/prompt_tokens")
                        .and_then(Value::as_u64),
                    output_tokens: value
                        .pointer("/usage/completion_tokens")
                        .and_then(Value::as_u64),
                },
            ))
        }
        LiveOperation::Responses => {
            let output = value
                .get("output")
                .and_then(Value::as_array)
                .context("Responses output")?;
            if value.get("object").and_then(Value::as_str) != Some("response")
                || value.get("model").and_then(Value::as_str) != Some(options.alias.as_str())
                || !matches!(
                    value.get("status").and_then(Value::as_str),
                    Some("completed" | "incomplete")
                )
                || output.is_empty()
                || output.len() > 64
            {
                bail!("invalid Responses shape");
            }
            Ok((
                None,
                SafeResultSummary {
                    object: "response".to_string(),
                    item_count: output.len(),
                    input_tokens: value.pointer("/usage/input_tokens").and_then(Value::as_u64),
                    output_tokens: value
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64),
                },
            ))
        }
    }
}

fn expected_contract(options: &LiveValidationOptions) -> Option<EmbeddingContract> {
    Some(EmbeddingContract {
        space_id: options.expected_space_id.clone()?,
        space_revision: options.expected_space_revision?,
        dimension: options.expected_dimension?,
    })
}

fn read_authorization(path: &Path) -> Result<HeaderValue> {
    let metadata = regular_file_metadata(path, "header file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("header file must not grant group or other permissions");
        }
    }
    let contents = fs::read_to_string(path).context("header-file read")?;
    let token = contents
        .strip_suffix('\n')
        .unwrap_or(&contents)
        .strip_prefix("Authorization: Bearer ")
        .filter(|value| {
            !value.is_empty()
                && !value
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        })
        .context("header file must contain exactly one bearer Authorization header")?;
    if contents.matches('\n').count() > 1 || contents.contains('\r') {
        bail!("header file must contain exactly one line");
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    Ok(value)
}

fn build_client(options: &LiveValidationOptions) -> Result<reqwest::Client> {
    let timeout = Duration::from_secs(options.timeout_seconds);
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(timeout)
        .timeout(timeout)
        .min_tls_version(reqwest::tls::Version::TLS_1_2);
    if let Some(path) = &options.ca_file {
        regular_file_metadata(path, "CA file")?;
        let pem = fs::read(path).context("CA-file read")?;
        for certificate in reqwest::Certificate::from_pem_bundle(&pem)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

fn regular_file_metadata(path: &Path, label: &str) -> Result<fs::Metadata> {
    let metadata = fs::metadata(path).with_context(|| format!("{label} metadata"))?;
    if !metadata.is_file() {
        bail!("{label} must resolve to a regular file");
    }
    Ok(metadata)
}

async fn read_bounded(
    mut response: reqwest::Response,
) -> std::result::Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("responseTooLarge");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "transport")? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("responseTooLarge");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn bounded_read_exit_code(category: &str) -> i32 {
    if category == "transport" { 13 } else { 26 }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .map(str::to_string)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_string(headers, name)?.parse().ok()
}

fn failure_category(
    operation: LiveOperation,
    status: u16,
    code: Option<&str>,
) -> (&'static str, i32) {
    match (status, code) {
        (401 | 403, _) => ("gatewayAuthorization", 21),
        (404, Some("model_not_found")) => ("aliasNotVisible", 20),
        (429, Some("rate_limit_exceeded")) => ("providerRateLimited", 23),
        (504, Some("provider_error")) => ("providerTimeout", 24),
        (400 | 502, Some("provider_error")) => ("providerError", 22),
        (400, Some("invalid_request" | "unsupported_feature")) => {
            (operation_contract_category(operation), 25)
        }
        (
            429 | 503,
            Some(
                "capacity_exhausted"
                | "budget_exhausted"
                | "model_unavailable"
                | "service_unavailable",
            ),
        ) => ("gatewayError", 26),
        _ => ("gatewayError", 26),
    }
}

fn operation_contract_category(operation: LiveOperation) -> &'static str {
    match operation {
        LiveOperation::Embeddings => "embeddingContract",
        LiveOperation::ChatCompletions | LiveOperation::Responses => "generationContract",
    }
}

fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn render_text(report: &LiveValidationReport) -> String {
    let operation = serde_json::to_value(report.operation)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "status={}\ncategory={}\noperation={}\nalias={}\nrequestId={}\nhttpStatus={}\nconfigGeneration={}\nbilledCostMicros={}",
        report.status,
        report.category,
        operation,
        report.alias,
        report.request_id.as_deref().unwrap_or(""),
        report.http_status,
        report
            .config_generation
            .map_or_else(String::new, |value| value.to_string()),
        report
            .billed_cost_micros
            .map_or_else(String::new, |value| value.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(operation: &str) -> Vec<String> {
        [
            "--gateway-url",
            "https://gateway.example.com",
            "--operation",
            operation,
            "--alias",
            "public-model",
            "--header-file",
            "/protected/gateway-header",
            "--timeout-seconds",
            "30",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn parser_accepts_only_the_closed_operation_contract() {
        let chat = parse_live_validation_options(&common("chat-completions")).unwrap();
        assert_eq!(chat.operation, LiveOperation::ChatCompletions);

        let mut embeddings = common("embeddings");
        embeddings.extend(
            [
                "--expected-space-id",
                "nvidia-nemotron-3-embed-1b",
                "--expected-space-revision",
                "1",
                "--expected-dimension",
                "2048",
            ]
            .into_iter()
            .map(str::to_string),
        );
        assert!(parse_live_validation_options(&embeddings).is_ok());

        for forbidden in [
            "--input",
            "--prompt",
            "--provider-url",
            "--provider-api-key",
            "--deployment",
            "--route",
            "--stream",
            "--concurrency",
        ] {
            let mut args = common("responses");
            args.extend([forbidden.to_string(), "value".to_string()]);
            assert!(parse_live_validation_options(&args).is_err(), "{forbidden}");
        }
    }

    #[test]
    fn parser_rejects_duplicate_output_and_invalid_embedding_space_ids() {
        let mut duplicate = common("responses");
        duplicate.extend(
            ["--output", "json", "--output", "text"]
                .into_iter()
                .map(str::to_string),
        );
        assert!(parse_live_validation_options(&duplicate).is_err());

        for invalid in ["contains space", "contains\nnewline", "", &"x".repeat(256)] {
            let mut embeddings = common("embeddings");
            embeddings.extend([
                "--expected-space-id".to_string(),
                invalid.to_string(),
                "--expected-space-revision".to_string(),
                "1".to_string(),
                "--expected-dimension".to_string(),
                "2048".to_string(),
            ]);
            assert!(
                parse_live_validation_options(&embeddings).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn protected_header_accepts_read_only_kubernetes_style_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("..data-header");
        let projected = directory.path().join("gateway-headers.txt");
        fs::write(&target, "Authorization: Bearer test-token\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
        symlink(&target, &projected).unwrap();

        assert!(regular_file_metadata(&projected, "header file").is_ok());
        assert!(read_authorization(&projected).is_ok());
    }

    #[test]
    fn gateway_failure_codes_map_to_the_frozen_operator_categories() {
        assert_eq!(bounded_read_exit_code("transport"), 13);
        assert_eq!(bounded_read_exit_code("responseTooLarge"), 26);
        assert_eq!(
            failure_category(LiveOperation::Embeddings, 400, Some("unsupported_feature")),
            ("embeddingContract", 25)
        );
        assert_eq!(
            failure_category(LiveOperation::Responses, 400, Some("unsupported_feature")),
            ("generationContract", 25)
        );
        for (status, code) in [
            (503, "model_unavailable"),
            (503, "service_unavailable"),
            (429, "capacity_exhausted"),
            (429, "budget_exhausted"),
        ] {
            assert_eq!(
                failure_category(LiveOperation::Embeddings, status, Some(code)),
                ("gatewayError", 26)
            );
        }
    }

    #[test]
    fn fixed_requests_are_exact_and_non_streaming() {
        let fixtures = [
            (
                LiveOperation::Embeddings,
                json!({"model":"demo","input":"light-gateway configuration validation"}),
            ),
            (
                LiveOperation::ChatCompletions,
                json!({"model":"demo","messages":[{"role":"user","content":"Reply with OK."}],"max_tokens":1,"stream":false}),
            ),
            (
                LiveOperation::Responses,
                json!({"model":"demo","input":"Reply with OK.","max_output_tokens":1,"stream":false}),
            ),
        ];
        for (operation, expected) in fixtures {
            let actual: Value = serde_json::from_slice(&fixed_request(operation, "demo")).unwrap();
            assert_eq!(actual, expected);
            assert_ne!(actual.get("stream"), Some(&Value::Bool(true)));
        }
    }

    #[test]
    fn gateway_origin_and_alias_validation_fail_closed() {
        for invalid in [
            "http://gateway.example.com",
            "https://user@gateway.example.com",
            "https://gateway.example.com/path",
            "https://gateway.example.com?query=1",
        ] {
            assert!(validate_gateway_url(invalid).is_err(), "{invalid}");
        }
        assert!(valid_alias("kb-query"));
        assert!(!valid_alias("../kb-query"));
        assert!(!valid_alias("kb/query"));
    }

    #[test]
    fn rendered_reports_never_include_model_output() {
        let report = LiveValidationReport {
            schema_version: SCHEMA_VERSION,
            status: "pass",
            category: "validated",
            operation: LiveOperation::Responses,
            alias: "demo".to_string(),
            timestamp: "2026-08-10T00:00:00Z".to_string(),
            request_id: Some("request-1".to_string()),
            http_status: 200,
            config_generation: None,
            billed_cost_micros: None,
            contract: None,
            result: Some(SafeResultSummary {
                object: "response".to_string(),
                item_count: 1,
                input_tokens: Some(3),
                output_tokens: Some(1),
            }),
        };
        let json = serde_json::to_string(&report).unwrap();
        let text = render_text(&report);
        for output in [json, text] {
            assert!(!output.contains(FIXED_GENERATION_TEXT));
            assert!(!output.contains("output_text"));
            assert!(!output.contains("Authorization"));
        }
    }
}
