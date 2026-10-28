//! Fixture wire dependencies only. All gateway behavior runs in production Init/service.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, task::JoinHandle};

struct WireState {
    replies: Vec<Value>,
    calls: Mutex<Vec<Value>>,
    errors: Mutex<Vec<String>>,
    invoke: bool,
    sdk_http: bool,
    base: String,
    timing: Mutex<Option<Arc<crate::probe_timing::Timeline>>>,
}
pub(crate) struct Dependency {
    pub url: String,
    state: Arc<WireState>,
    task: JoinHandle<()>,
}
impl Drop for Dependency {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Dependency {
    pub fn observe_timing(&self, timing: Arc<crate::probe_timing::Timeline>) {
        *self.state.timing.lock().unwrap() = Some(timing);
    }
    pub fn calls(&self) -> Vec<Value> {
        self.state.calls.lock().unwrap().clone()
    }
    pub fn errors(&self) -> Vec<String> {
        self.state.errors.lock().unwrap().clone()
    }
    /// Reserve an owned loopback port without listening. A connection must be
    /// refused, and another process cannot claim the fixture endpoint meanwhile.
    pub async fn start_refused() -> Result<Self> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind("127.0.0.1:0".parse()?)?;
        let url = format!("http://{}", socket.local_addr()?);
        let state = Arc::new(WireState {
            replies: vec![],
            calls: Mutex::new(vec![]),
            errors: Mutex::new(vec![]),
            invoke: false,
            sdk_http: false,
            base: url.clone(),
            timing: Mutex::new(None),
        });
        let task = tokio::spawn(async move {
            let _socket = socket;
            std::future::pending::<()>().await;
        });
        Ok(Self { url, state, task })
    }
    pub async fn start(replies: Vec<Value>, invoke: bool) -> Result<Self> {
        Self::start_with_wire(replies, invoke, false).await
    }
    pub async fn start_with_wire(
        replies: Vec<Value>,
        invoke: bool,
        sdk_http: bool,
    ) -> Result<Self> {
        Self::start_transport(replies, invoke, sdk_http, false).await
    }
    pub async fn start_tls(replies: Vec<Value>, invoke: bool) -> Result<Self> {
        Self::start_transport(replies, invoke, false, true).await
    }
    async fn start_transport(
        replies: Vec<Value>,
        invoke: bool,
        sdk_http: bool,
        tls: bool,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!(
            "{}://{}",
            if tls { "https" } else { "http" },
            listener.local_addr()?
        );
        let state = Arc::new(WireState {
            replies,
            calls: Mutex::new(vec![]),
            errors: Mutex::new(vec![]),
            invoke,
            sdk_http,
            base: url.clone(),
            timing: Mutex::new(None),
        });
        async fn handle(State(s): State<Arc<WireState>>, request: Request) -> Response {
            let timing = s.timing.lock().unwrap().clone();
            if let Some(timing) = &timing {
                timing.record("invoke-received");
            }
            let (parts, body) = request.into_parts();
            let body = match to_bytes(body, 8_000_000).await {
                Ok(body) => body,
                Err(_) => return http::StatusCode::BAD_REQUEST.into_response(),
            };
            let mut call = if s.invoke {
                let raw = parts
                    .uri
                    .path()
                    .strip_prefix("/2015-03-31/functions/")
                    .and_then(|s| s.strip_suffix("/invocations"))
                    .unwrap_or("");
                let target = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
                match serde_json::from_slice::<Value>(&body) {
                    Ok(event) => {
                        json!({"target":target,"event":event,"invoke":{"logType":parts.headers.get("x-amz-log-type").and_then(|v|v.to_str().ok()).unwrap_or("None")}})
                    }
                    Err(_) => {
                        s.errors
                            .lock()
                            .unwrap()
                            .push("Invoke payload is not JSON".into());
                        return http::StatusCode::BAD_REQUEST.into_response();
                    }
                }
            } else {
                let mut headers = serde_json::Map::new();
                for key in parts.headers.keys() {
                    headers.insert(
                        key.to_string(),
                        json!(
                            parts
                                .headers
                                .get_all(key)
                                .iter()
                                .map(|v| v.to_str().unwrap_or("<invalid>"))
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                json!({"target":format!("{}{}",s.base,parts.uri),"method":parts.method.as_str(),"uri":parts.uri.to_string(),"body":String::from_utf8_lossy(&body),"serviceId":parts.headers.get("service_id").and_then(|v|v.to_str().ok()),"headers":headers})
            };
            if s.invoke && s.sdk_http {
                call["sdkHttp"] = json!({"method":parts.method.as_str(),"uri":parts.uri.to_string(),"endpoint":s.base});
            }
            let index = {
                let mut calls = s.calls.lock().unwrap();
                let index = calls.len();
                calls.push(call.take());
                index
            };
            let Some(reply) = s.replies.get(index) else {
                s.errors
                    .lock()
                    .unwrap()
                    .push("undeclared dependency call".into());
                return http::StatusCode::BAD_GATEWAY.into_response();
            };
            let mut headers = http::HeaderMap::new();
            let (status, body) = if s.invoke {
                let actual = s.calls.lock().unwrap()[index]["target"].clone();
                let matched = if reply["match"].get("target").is_some() {
                    reply["match"]["target"] == actual
                } else {
                    reply["match"]["targets"]
                        .as_array()
                        .is_some_and(|a| a.len() == 2 && a[0] != a[1] && a.contains(&actual))
                };
                if !matched {
                    s.errors
                        .lock()
                        .unwrap()
                        .push("Invoke target does not match fixture replay".into());
                }
                if let Some(failure) = reply["respond"].get("sdkFailure") {
                    if failure["kind"] == "SdkClientException" {
                        // A broken response body is a real client transport error,
                        // not a fabricated Lambda service-error document. The SDK
                        // owns classification and the production backend maps it.
                        let stream = futures_util::stream::once(async {
                            Err::<Bytes, _>(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "fixture transport reset",
                            ))
                        });
                        return Response::new(axum::body::Body::from_stream(stream));
                    }
                    // Standard Lambda REST service-error response. The production
                    // SDK and backend classifier own interpretation and retries.
                    let kind = failure["kind"].as_str().unwrap_or("");
                    let status = failure["statusCode"].as_u64().unwrap_or(500) as u16;
                    return (
                        http::StatusCode::from_u16(status).unwrap(),
                        [("x-amzn-errortype", kind)],
                        axum::Json(json!({"message":failure["message"],"Type":"User"})),
                    )
                        .into_response();
                }
                if let Some(error) = reply["respond"]["functionError"].as_str() {
                    headers.insert("x-amz-function-error", error.parse().unwrap());
                }
                headers.insert("content-type", "application/json".parse().unwrap());
                if let Some(version) = reply["respond"]["executedVersion"].as_str() {
                    headers.insert("x-amz-executed-version", version.parse().unwrap());
                }
                if let Some(log) = reply["respond"]["logResult"].as_str() {
                    headers.insert("x-amz-log-result", log.parse().unwrap());
                }
                (
                    200,
                    serde_json::to_vec(&reply["respond"]["payload"]).unwrap(),
                )
            } else {
                let call = &s.calls.lock().unwrap()[index];
                if call["method"] != reply["method"] || call["uri"] != reply["uri"] {
                    s.errors
                        .lock()
                        .unwrap()
                        .push("HTTP method/URI does not match fixture responder".into());
                }
                for pair in reply["headers"].as_array().unwrap() {
                    headers.append(
                        http::HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap(),
                        pair[1].as_str().unwrap().parse().unwrap(),
                    );
                }
                (
                    reply["status"].as_u64().unwrap() as u16,
                    reply["body"].as_str().unwrap().as_bytes().to_vec(),
                )
            };
            if let Some(ms) = reply["delayMs"].as_u64() {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            if let Some(timing) = &timing {
                timing.record("invoke-response-ready");
            }
            (
                http::StatusCode::from_u16(status).unwrap(),
                headers,
                Bytes::from(body),
            )
                .into_response()
        }
        let router = Router::new().fallback(handle).with_state(state.clone());
        let task = if tls {
            let certificate =
                rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
            let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(
                    certificate.signing_key.serialize_der(),
                )
                .into(),
            )?;
            let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
            let listener = listener.into_std()?;
            tokio::spawn(async move {
                let _ = axum_server::from_tcp_rustls(listener, tls)
                    .serve(router.into_make_service())
                    .await;
            })
        } else {
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            })
        };
        Ok(Self { url, state, task })
    }
}

/// Closed dependency substitution. Never changes the event supplied to service.
pub(crate) fn substitute(value: &mut Value, base: &str) {
    match value {
        Value::String(s) => *s = s.replace("${dependency:http}", base),
        Value::Array(a) => {
            for v in a {
                substitute(v, base)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                substitute(v, base)
            }
        }
        _ => {}
    }
}

pub(crate) fn cases(f: &Value) -> Vec<&Value> {
    f["steps"]
        .as_array()
        .map(|s| s.iter().collect())
        .unwrap_or_else(|| vec![f])
}

/// Closed Lambda context recipe; no clock replacement or deadline tolerance.
pub(crate) fn valid_context(context: &Value) -> bool {
    context.as_object().is_some_and(|c| {
        !c.is_empty()
            && c.iter().all(|(k, v)| match k.as_str() {
                "functionName" => v.as_str().is_some_and(|s| !s.is_empty()),
                "remainingTimeMs" => v.as_u64().is_some_and(|ms| ms <= 900_000),
                _ => false,
            })
    })
}

pub async fn run(f: &Value, revision: &str) -> Result<Value> {
    if crate::single_runner::supported(f) && !crate::gate_component::supported(f) {
        return crate::single_runner::run(f, revision).await;
    }
    if crate::emission_worker::needed(f)
        && std::env::var("LIGHT_LAMBDA_EMISSION_WORKER").as_deref() != Ok("1")
    {
        return crate::emission_worker::run(f, revision).await;
    }
    if f.get("insecureTestConfigPrerequisite").is_some()
        && std::env::var("LIGHT_LAMBDA_CONFORMANCE_TEST_WORKER").as_deref() != Ok("1")
    {
        return crate::test_config_worker::run(f, revision).await;
    }
    let _application_lock = crate::audit_capture::APPLICATION_LOCK.lock().await;
    crate::audit_capture::start()?;
    let start = std::time::Instant::now();
    let dir = tempfile::tempdir()?;
    let mut tokens = crate::tokens::Tokens::start(f).await?;
    let cases = cases(f);
    let mut replies: Value = json!(
        cases
            .iter()
            .flat_map(|s| s["backend"].as_array().into_iter().flatten().cloned())
            .collect::<Vec<_>>()
    );
    tokens.resolve(&mut replies);
    let replies = replies.as_array().unwrap().clone();
    let sdk_profile = f.get("dependencies").and_then(|d| d.get("invoke"));
    let replies = if let Some(profile) = sdk_profile {
        ensure!(
            (profile.get("tls").is_none()
                && matches!(
                    f["id"].as_str(),
                    Some(
                        "D-08-zero-remaining-multi-contract-v1"
                            | "D-08-zero-remaining-supported-config-v1"
                    )
                ))
                || (profile["tls"] == true
                    && f["id"] == "CFG-02-sdk-netty-trust-boundary-represented-v1"),
            "unsupported Invoke profile"
        );
        profile["responses"].as_array().context("Invoke responses")?.iter().map(|reply| {
            json!({"match":{"target":"test-only-sdk"},"respond":{"payload":reply["payload"]},"delayMs":reply["delayMs"]})
        }).collect()
    } else {
        replies
    };
    let invoke = if sdk_profile.is_some_and(|p| p["tls"] == true) {
        Dependency::start_tls(replies, true).await?
    } else {
        Dependency::start_with_wire(replies, true, sdk_profile.is_some()).await?
    };
    let http = if f.pointer("/dependencies/http/refused") == Some(&json!(true)) {
        ensure!(
            f["dependencies"]["http"]["responses"] == json!([]),
            "refused endpoint cannot replay responses"
        );
        Some(Dependency::start_refused().await?)
    } else if f["dependencies"]["http"]["tls"] == true {
        Some(
            Dependency::start_tls(
                f["dependencies"]["http"]["responses"]
                    .as_array()
                    .context("TLS responses")?
                    .clone(),
                false,
            )
            .await?,
        )
    } else if f.pointer("/dependencies/http").is_some() {
        Some(
            Dependency::start(
                {
                    let mut replies = f["dependencies"]["http"]["responses"].clone();
                    tokens.resolve(&mut replies);
                    replies.as_array().context("HTTP responses")?.clone()
                },
                false,
            )
            .await?,
        )
    } else {
        None
    };
    let ldap = if let Some(declaration) = f["dependencies"].get("ldap") {
        Some(crate::ldap_dependency::Ldap::start(declaration).await?)
    } else {
        None
    };
    let mut config = f["config"].clone();
    if let Some(ldap) = &ldap {
        crate::ldap_dependency::substitute(&mut config, &ldap.url);
    }

    tokens.resolve(&mut config);
    if f.get("loopbackOauthPrerequisite").is_some() {
        ensure!(
            f["loopbackOauthPrerequisite"] == "synthetic-client-token-cache-v1"
                && config["files"].get("cache").is_none(),
            "unsupported or shadowed OAuth cache prerequisite"
        );
        // Same closed named-cache infrastructure as the native prerequisite;
        // production TokenPolicyProvider still owns validation and cache behavior.
        config["files"]["cache"] = json!({"enabled":false,"caches":[{"cacheName":"token","expiryInMinutes":15,"maxSize":100}]});
    }
    if f.get("ruleConfigurationPair").is_some() {
        ensure!(
            f["ruleConfigurationPair"]["rust"]["executor"] == "light-rule-cel",
            "unsupported Rust rule executor recipe"
        );
        let rule = &f["ruleConfigurationPair"]["rust"]["rule"];
        ensure!(rule.is_object(), "missing Rust immutable rule program");
        ensure!(
            config["files"].get("rule").is_none(),
            "ambiguous fixture rule program"
        );
        config["files"]["rule"] = rule.clone();
    }
    if let Some(http) = &http {
        substitute(&mut config, &http.url);
    }
    let blocking_probe = crate::blocking_probe::configure(f, &mut config)?;
    let completion_probe = crate::completion_probe::configure(f, &mut config)?;
    let work = crate::bounded_work::configure(f, &mut config)?;
    if let Some(work) = &work {
        invoke.observe_timing(work.timing.clone());
    }
    let files = config["files"].as_object().context("config files")?;
    for (name, value) in files {
        ensure!(
            name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "unsafe fixture file name"
        );
        if name != "values" {
            let mut value = value.clone();
            if name == "lambda-proxy" {
                value["endpointOverride"] = json!(invoke.url);
            }
            let bytes = if let Some(text) = value.as_str() {
                text.as_bytes().to_vec()
            } else {
                serde_json::to_vec(&value)?
            };
            std::fs::write(dir.path().join(format!("{name}.yml")), bytes)?;
        }
    }
    let mut values = files.get("values").cloned().unwrap_or(json!({}));
    for (k, v) in config["values"].as_object().context("values")? {
        values[k] = v.clone();
    }
    values["lambda-proxy.endpointOverride"] = json!(invoke.url);
    std::fs::write(dir.path().join("values.yml"), serde_json::to_vec(&values)?)?;
    if f.get("configurationProbe").is_some() {
        ensure!(
            f["configurationProbe"] == "syntheticDecryption"
                && f["id"] == "CFG-03-synthetic-decryption-init-probe-v1",
            "closed decryption prerequisite"
        );
        std::fs::write(
            dir.path().join("test-only-crypt.yml"),
            "value: CRYPT:01010101010101010101010101010101:020202020202020202020202:384c3e9b2a30679cf57d653304f96af7599f065069b95682eb8275ee23e927a00d53a009e193590afa\n",
        )?;
    }
    let mut input = light_lambda::input(Some(dir.path().into()), false)
        .map_err(|e| anyhow::anyhow!(e.message()))?;
    if let Some(work) = &work {
        crate::bounded_work::install(&mut input, work);
    }
    if let Some(capture) = &completion_probe {
        crate::completion_probe::install(&mut input, capture);
    }
    if let Some(capture) = &blocking_probe {
        crate::blocking_probe::install(&mut input, capture);
    }
    let failed_router = crate::router_failed_component::install(f, &mut input)?;
    let response_counts = crate::policy_observer::install(f, &mut input, work.as_ref())?;
    let transformer_input = crate::transformer_observer::install(f, &mut input)?;
    input.local_dir = dir.path().join("no-local-config");
    input.default_dir = None;
    input.gateway_identity = serverless_aws::target::GatewayIdentity {
        function_name: "offline-validator".into(),
        region: "us-east-1".into(),
        account_id: "000000000000".into(),
    };
    if let Some(name) = f["runtimeIdentity"]["functionName"]
        .as_str()
        .or_else(|| f["context"]["functionName"].as_str())
    {
        input.gateway_identity.function_name = name.to_owned();
    }
    input.environment.function_name = input.gateway_identity.function_name.clone();
    // input() constructed this provider with its initial offline identity.
    // Rebind the same production factory to the fixture's trusted runtime name.
    for registration in &mut input.handlers {
        if registration.id == serverless_core::HandlerId::Metrics {
            registration.factory = Some(Arc::new(
                light_lambda_runtime::metrics_policy::MetricsPolicyProvider {
                    function_name: input.environment.function_name.clone(),
                },
            ));
        }
    }
    input.environment.region = Some("us-east-1".into());
    input.environment.allow_insecure_test_endpoints = true;
    let prepared = match light_lambda_runtime::init::prepare_local(&input).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return init_result(
                f,
                revision,
                start,
                error,
                (&invoke, http.as_ref(), &tokens, dir.path(), ldap.as_ref()),
            )
            .await;
        }
    };
    let configuration_observation = if f.get("configurationProbe").is_some() {
        let actual: Value = prepared
            .module_registry
            .load_config(&prepared, "test-only-crypt.yml")?;
        Some(json!({"format":"CRYPT:salt:iv:ciphertext","decrypted":actual["value"]}))
    } else {
        None
    };
    let proxy: Value = prepared
        .module_registry
        .load_config(&prepared, "lambda-proxy.yml")?;
    let region = proxy["region"]
        .as_str()
        .filter(|r| !r.is_empty())
        .unwrap_or("us-east-1")
        .to_owned();
    input.gateway_identity.region = region.clone();
    input.environment.region = Some(region.clone());
    input.backend = light_lambda_runtime::init::BackendConfiguration::Sdk(Box::new(
        aws_sdk_lambda::config::Builder::new()
            .behavior_version(aws_sdk_lambda::config::BehaviorVersion::latest())
            .region(aws_sdk_lambda::config::Region::new(region))
            .credentials_provider(aws_sdk_lambda::config::Credentials::new(
                "test-only",
                "test-only",
                None,
                None,
                "fixture",
            ))
            .build(),
    ));
    let snapshot = light_lambda_runtime::init::initialize(input).await;
    let mut observations = vec![];
    let mut invocation_windows = vec![];
    let mut bounded_work_timing = vec![];
    let mut signing_epochs = vec![];
    let mut errors = vec![];
    let mut initialization = None;
    match snapshot {
        Err(error) => {
            return init_result(
                f,
                revision,
                start,
                error,
                (&invoke, http.as_ref(), &tokens, dir.path(), ldap.as_ref()),
            )
            .await;
        }
        Ok(snapshot) => {
            if let Some(capture) = &failed_router {
                return crate::router_failed_component::run(
                    f,
                    revision,
                    capture,
                    &tokens,
                    &invoke,
                    http.as_ref(),
                )
                .await;
            }
            if crate::gate_component::supported(f) {
                return crate::gate_component::run(f, revision, dir.path(), &tokens, &invoke).await;
            }
            let (snapshot, trace_observer) = if f["traceContextProbe"] == "dispatch-v1" {
                let (snapshot, capture) = crate::trace_observer::install(&snapshot);
                (snapshot, Some(capture))
            } else {
                (snapshot, None)
            };
            if f.get("expectRustInit").is_some()
                || f.get("expectInit").is_some()
                || f.get("expectJavaInit").is_some()
                || f["initOnly"] == true
            {
                let mut actual = json!({"outcome":"initSuccess","backendCalls":invoke.calls(),"jwksFetches":tokens.calls().len(),"jwksCalls":tokens.calls()});
                if let Some(actual_configuration) = &configuration_observation {
                    actual["configurationObservation"] = actual_configuration.clone();
                }
                if f["expect"].get("configurationWarningObservation").is_some() {
                    actual["configurationWarningObservation"] =
                        crate::audit_capture::configuration_warning()?;
                }
                if let Some(expected) = f
                    .get("expectRustInit")
                    .or_else(|| f.get("expectInit"))
                    .or_else(|| (f["initOnly"] == true).then_some(&f["expect"]))
                    && let Err(error) = crate::incremental::assert_observation(expected, &actual)
                {
                    errors.push(format!("Init: {error:#}"));
                }
                actual
                    .as_object_mut()
                    .unwrap()
                    .remove("configurationWarningObservation");
                actual
                    .as_object_mut()
                    .unwrap()
                    .remove("configurationObservation");
                initialization = Some(actual);
            }
            if let Some(ids) = f.get("jwksPublishedKeys") {
                tokens.publish(ids)?;
            }
            for (index, case) in cases.iter().enumerate() {
                if f["initOnly"] == true {
                    let mut observed = initialization.clone().context("Init-only observation")?;
                    if f["expect"].get("configurationWarningObservation").is_some() {
                        observed["configurationWarningObservation"] =
                            crate::audit_capture::configuration_warning()?;
                    }
                    if let Some(configuration) = &configuration_observation {
                        observed["configurationObservation"] = configuration.clone();
                    }
                    observations.push(observed);
                    break;
                }
                if let Some(files) = case["warmConfig"].as_object() {
                    for (name, value) in files {
                        ensure!(
                            name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
                            "unsafe warm fixture file name"
                        );
                        let bytes = if let Some(text) = value.as_str() {
                            text.as_bytes().to_vec()
                        } else {
                            serde_json::to_vec(value)?
                        };
                        std::fs::write(dir.path().join(format!("{name}.yml")), bytes)?;
                    }
                }
                if let Some(ms) = case["waitBeforeMs"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                ensure!(
                    case["cacheActions"].as_array().is_none_or(Vec::is_empty),
                    "cached-claim mutation is not admitted on a successful Rust Init"
                );
                tokens.sign(Some(index))?;
                if let Some(ids) = case.get("jwksPublishedKeys") {
                    tokens.publish(ids)?;
                }
                let mut case = (*case).clone();
                if crate::authorizer::needed(f) {
                    let epochs = tokens.signing_epochs();
                    case["expect"] = crate::authorizer::bind(&case["expect"], f, Some(&epochs))?;
                    signing_epochs.push(json!({"step":index,"epochs":epochs}));
                }
                tokens.resolve(&mut case);
                let trace_before = trace_observer.as_ref().map_or(0, |capture| capture.count());
                let rule_failures_before = crate::audit_capture::rule_failures()?;
                let configuration_before = crate::audit_capture::configuration_diagnostics()?.len();
                let work_before = work.as_ref().map_or(0, |w| w.entries());
                let jwks_before = tokens.calls().len();
                let before = invoke.calls().len();
                let http_before = http.as_ref().map_or(0, |h| h.calls().len());
                let mut context = lambda_runtime::Context::default();
                context.request_id = if index == 0 {
                    "test-only-request".into()
                } else {
                    format!("test-only-request-{index}")
                };
                let recipe = case.get("context").or_else(|| f.get("context"));
                let remaining = recipe
                    .and_then(|c| c["remainingTimeMs"].as_u64())
                    .unwrap_or(10_000);
                if let Some(work) = &work {
                    work.timing.begin(remaining);
                }
                // The isolated response-timeout recipe measures pipeline semantics,
                // not OS/loopback scheduling latency. Keep virtual time stationary
                // until the response probe has actually entered; otherwise Tokio's
                // idle auto-advance could expire the SDK deadline during socket IO.
                let controlled_response_clock = f["boundedWorkProbe"] == "response-100ms-v1"
                    && crate::emission_worker::needed(f)
                    && std::env::var("LIGHT_LAMBDA_EMISSION_WORKER").as_deref() == Ok("1");
                let clock_driver = if controlled_response_clock {
                    tokio::time::pause();
                    let work = work.as_ref().context("response timeout work")?.clone();
                    let expected_entries =
                        work.entries() + f["concurrentCopies"].as_u64().unwrap_or(1) as usize;
                    Some(tokio::spawn(async move {
                        while work.entries() < expected_entries {
                            tokio::task::yield_now().await;
                        }
                    }))
                } else {
                    None
                };
                context.deadline = (SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
                    as u64)
                    .checked_add(remaining)
                    .context("Lambda deadline overflow")?;
                let window_start = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
                let invocation_start = std::time::Instant::now();
                let copies = f["concurrentCopies"].as_u64().unwrap_or(1) as usize;
                ensure!(copies == 1 || copies == 2, "closed caller count");
                let responses = futures_util::future::join_all((0..copies).map(|copy| {
                    let snapshot = snapshot.clone();
                    let event = case["rawEvent"].clone();
                    let mut context = context.clone();
                    if copies > 1 {
                        context.request_id = format!("test-only-request-{copy}");
                    }
                    let timing = work.as_ref().map(|work| work.timing.clone());
                    async move {
                        if let Some(timing) = &timing {
                            timing.record("service-start");
                        }
                        light_lambda_runtime::service::handle_with_guard(
                            snapshot,
                            lambda_runtime::LambdaEvent::new(event, context),
                            Some(&light_lambda::phase1_guard()),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("service execution failed"))
                    }
                }))
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>();
                if let Some(driver) = clock_driver {
                    driver.abort();
                    tokio::time::resume();
                }
                let responses = responses?;
                if let Some(work) = &work {
                    work.timing.record("service-return");
                    bounded_work_timing.push(work.timing.observation(index));
                }
                let response = &responses[0];
                let invocation_duration = invocation_start.elapsed().as_millis() as u64;
                let window = json!({"step":index,"startEpochMs":window_start,"endEpochMs":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64});
                if crate::recipes::needs_clock(&case) || case["expect"].get("emf").is_some() {
                    invocation_windows.push(window.clone());
                }
                let mut observed = if copies == 1 {
                    let mut actual = alb_observation(response)?;
                    actual["headersAbsent"] = json!([]);
                    actual["responseMaps"] = json!({"headers":response["headers"],"multiValueHeaders":response["multiValueHeaders"]});
                    actual
                } else {
                    json!({"outcome":"parallelResponses","responses":responses.iter().map(alb_observation).collect::<Result<Vec<_>>>()?})
                };
                observed["backendCalls"] = json!(invoke.calls()[before..]);
                observed["jwksFetches"] = json!(tokens.calls().len() - jwks_before);
                if let Some(capture) = &blocking_probe {
                    observed["blockingLease"] = capture.observation().await?;
                }
                if case["expect"].get("configurationDiagnostics").is_some() {
                    observed["configurationDiagnostics"] = json!({"multiValueRequiredLogs":crate::audit_capture::configuration_diagnostics()?.len()-configuration_before});
                }
                if let Some(counts) = &response_counts {
                    observed["responsePolicyObservation"] = counts.observation(
                        f["responsePolicyProbe"]["handler"]
                            .as_str()
                            .context("response observer name")?,
                    );
                }
                if let Some(work) = &work {
                    if let Some(mode) = f.get("requestFailureProbe") {
                        observed["failureProbeResult"] =
                            json!({"mode":mode,"executions":work.entries()-work_before});
                    } else {
                        observed["boundedWorkProbeExecutions"] =
                            json!(work.entries() - work_before);
                    }
                }
                if case["expect"].get("invocationTiming").is_some()
                    || case["expectJava"].get("invocationTiming").is_some()
                {
                    observed["invocationDurationMs"] = json!(invocation_duration);
                }
                if f.get("loopbackJwksPrerequisite").is_some() {
                    observed["jwksCalls"] = json!(tokens.calls()[jwks_before..]);
                }
                if let Some(ldap) = &ldap {
                    observed["ldapBinds"] = json!(ldap.binds());
                }
                if let Some(http) = &http {
                    observed["httpCalls"] = json!(http.calls()[http_before..]);
                }
                if let Some(capture) = &completion_probe {
                    observed["completionProbeResult"] = capture.observation()?;
                }
                if let Some(capture) = &trace_observer {
                    observed["traceContext"] = capture.observation(trace_before)?;
                }
                if f.get("ruleFailureProbe").is_some() {
                    observed["ruleFailuresCaught"] =
                        json!(crate::audit_capture::rule_failures()? - rule_failures_before);
                }
                if let Some(capture) = &transformer_input {
                    observed["ruleInputObservation"] = capture.observation();
                }
                if f["observeBackendDiagnostics"] == true {
                    observed["backendDiagnostics"] = crate::audit_capture::backend_diagnostics()?;
                }
                if f.get("securityLogProbe").is_some() {
                    ensure!(
                        f["securityLogProbe"] == "jwt-trace-v1",
                        "unknown security observation"
                    );
                    let recipes = f["tokens"].as_object().context("signed token recipes")?;
                    ensure!(
                        recipes.len() == 1,
                        "security observation needs one signed token"
                    );
                    let name = recipes.keys().next().unwrap();
                    let mut token = json!(format!("${{token:{name}}}"));
                    tokens.resolve(&mut token);
                    observed["securityLogObservation"] =
                        crate::audit_capture::security_observation(
                            token.as_str().context("signed token witness")?,
                        )?;
                }
                if case["observeAudit"] == true {
                    observed["auditRecords"] = crate::audit_capture::completion().await?;
                }
                let mut expected = case["expect"].clone();
                if std::env::var("LIGHT_LAMBDA_EMISSION_WORKER").as_deref() == Ok("1") {
                    // Only raw stdout assertion is deferred to the parent, which
                    // receives the bytes and reasserts the full original contract.
                    expected
                        .as_object_mut()
                        .context("expectation")?
                        .remove("emf");
                }
                if let Some(http) = &http {
                    substitute(&mut expected, &http.url);
                }
                if let Err(error) = crate::incremental::assert_observation_with_window(
                    &expected,
                    &observed,
                    Some(&window),
                ) {
                    errors.push(format!("step {index}: {error:#}"));
                }
                observations.push(observed);
            }
        }
    }
    errors.extend(invoke.errors());
    if let Some(http) = &http {
        errors.extend(http.errors());
    }
    let mut result = json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":start.elapsed().as_millis() as u64,"observations":observations,"assertionErrors":errors});
    if !bounded_work_timing.is_empty() {
        result["boundedWorkTiming"] = json!(bounded_work_timing);
    }
    if let Some(actual) = initialization {
        result["initialization"] = actual;
    }
    if !signing_epochs.is_empty() {
        result["tokenSigningEpochs"] = json!(signing_epochs);
    }
    if !invocation_windows.is_empty() {
        result["invocationWindows"] = json!(invocation_windows);
    }
    if cases
        .iter()
        .any(|c| c["expect"].get("configurationDiagnostics").is_some())
    {
        result["configurationDiagnosticEvidence"] =
            json!(crate::audit_capture::configuration_diagnostics()?);
    }
    if f["expect"].get("configurationWarningObservation").is_some() {
        result["configurationWarningEvidence"] =
            crate::audit_capture::configuration_warning_evidence()?;
    }
    tokens.redact(&mut result);
    Ok(result)
}

async fn init_result(
    f: &Value,
    revision: &str,
    start: std::time::Instant,
    error: light_lambda_runtime::init_error::InitError,
    dependencies: (
        &Dependency,
        Option<&Dependency>,
        &crate::tokens::Tokens,
        &std::path::Path,
        Option<&crate::ldap_dependency::Ldap>,
    ),
) -> Result<Value> {
    let (invoke, http, tokens, directory, ldap) = dependencies;
    let mut observed = json!({"outcome":"initError","code":error.error_type(),"backendCalls":invoke.calls(),"jwksFetches":tokens.calls().len(),"httpCalls":http.map_or_else(Vec::new, Dependency::calls)});
    if f.get("loopbackJwksPrerequisite").is_some()
        || f.get("expectInit")
            .and_then(|e| e.get("jwksCalls"))
            .is_some()
        || f["expect"].get("jwksCalls").is_some()
    {
        observed["jwksCalls"] = json!(tokens.calls());
    }
    if crate::audit_capture::enabled(f) {
        observed["auditRecords"] = crate::audit_capture::take()?;
    }
    if f["expect"].get("initFailureReporting").is_some() {
        observed["initFailureReporting"] = crate::bootstrap_probe::report(f, directory).await?;
    }
    if let Some(ldap) = ldap {
        observed["ldapBinds"] = json!(ldap.binds());
    }
    let mut errors = match crate::incremental::assert_observation(
        f.get("expectRustInit")
            .or_else(|| f.get("expectInit"))
            .unwrap_or(&f["expect"]),
        &observed,
    ) {
        Ok(()) => vec![],
        Err(assertion) => vec![format!(
            "Init rejected fixture configuration: {}; expectation mismatch: {assertion:#}",
            error.message()
        )],
    };
    errors.extend(invoke.errors());
    if let Some(http) = http {
        errors.extend(http.errors());
    }
    let mut result = json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":start.elapsed().as_millis() as u64,"observations":[observed],"assertionErrors":errors});
    if f.get("expectRustInit").is_some() {
        result["initialization"] = observed.clone();
        let observations = cases(f).iter().map(|case| {
            let mut observation = json!({"outcome":"notExecuted","reason":"initError","backendCalls":[],"jwksFetches":0,"httpCalls":[]});
            if case["observeAudit"] == true { observation["auditRecords"] = observed["auditRecords"].clone(); }
            if case["expect"].get("cacheActions").is_some() { observation["cacheActions"] = json!([]); }
            observation
        }).collect::<Vec<_>>();
        for (index, (case, actual)) in cases(f).iter().zip(&observations).enumerate() {
            if let Err(error) = crate::incremental::assert_observation(&case["expect"], actual) {
                result["assertionErrors"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(format!("step {index}: {error:#}")));
                result["execution"] = json!("failed");
            }
        }
        result["observations"] = json!(observations);
    } else if f.get("expectInit").is_some() || f.get("expectJavaInit").is_some() {
        result["initialization"] = result["observations"][0].clone();
    }
    tokens.redact(&mut result);
    Ok(result)
}

fn alb_observation(response: &Value) -> Result<Value> {
    let mut pairs = vec![];
    if let Some(single) = response["headers"].as_object() {
        for (key, value) in single {
            pairs.push(json!([key, value]));
        }
    }
    if let Some(multi) = response["multiValueHeaders"].as_object() {
        for (key, values) in multi {
            for value in values.as_array().context("ALB values")? {
                pairs.push(json!([key, value]));
            }
        }
    }
    Ok(
        json!({"outcome":"response","status":response["statusCode"],"headers":pairs,"body":response["body"],"bodyBase64":response["isBase64Encoded"]}),
    )
}
