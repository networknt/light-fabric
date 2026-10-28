//! Owned resource/precision composition; no production Validator registration.

#[tokio::test]
#[ignore = "owned precision vocabulary composition; requires WP12_PRECISION_BINARY and WP12_VOCABULARY_CASES"]
async fn vocabulary_amendments_through_real_service() {
    let binary = std::path::PathBuf::from(std::env::var_os("WP12_PRECISION_BINARY").unwrap());
    let cases: Value = serde_json::from_slice(
        &std::fs::read(std::env::var_os("WP12_VOCABULARY_CASES").unwrap()).unwrap(),
    )
    .unwrap();
    for case in cases["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        files(dir.path());
        std::fs::write(
            dir.path().join("lambda-proxy.yml"),
            "functions:\n  /x@post: TestFn\nregion: us-east-1\n",
        )
        .unwrap();
        let selected = dir.path().join("openapi.json");
        std::fs::write(&selected, case["source"].as_str().unwrap()).unwrap();
        let child = Arc::new(Mutex::new(None));
        let entries = vec![
            HandlerRegistration {
                id: HandlerId::Validator,
                config_file: "openapi-validator.yml",
                factory: Some(Arc::new(VocabularyRuntimeConsumer(child.clone()))),
            },
            HandlerRegistration {
                id: HandlerId::Specification,
                config_file: "openapi-handler.yml",
                factory: Some(Arc::new(VocabularyProducer(
                    PrecisionProducer {
                        binary: binary.clone(),
                        child: child.clone(),
                        captured: Arc::new(Mutex::new(None)),
                        selected,
                    },
                    case["expectedRootCount"].as_u64().unwrap_or(1) as usize,
                    case["expectedRequestSchemaCount"].as_u64().unwrap_or(1) as usize,
                ))),
            },
        ];
        let counts = Arc::new(PrecisionDispatchCounts::default());
        let mut init = input(dir.path(), entries, false);
        init.backend = BackendConfiguration::Injected(counts.clone());
        init.finalizers = vec![counts.clone()];
        let result = initialize(init).await;
        if case["expectInitRejection"] == true {
            assert!(result.is_err(), "{id}");
            assert_eq!(counts.dispatches.load(Ordering::SeqCst), 0);
            continue;
        }
        let snapshot = result.unwrap();
        for _ in 0..2 {
            let payload = json!({"path":"/x","httpMethod":"POST","multiValueHeaders":{"Content-Type":["application/json"]},"body":case["requestBody"],"isBase64Encoded":false,"requestContext":{"elb":{"targetGroupArn":"arn:aws:elasticloadbalancing:us-east-1:000000000000:targetgroup/test-only/0000000000000000"}}});
            let mut context = lambda_runtime::Context::default();
            context.request_id = "owned-vocabulary".into();
            context.deadline = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 5000;
            let reply = light_lambda_runtime::service::handle(
                snapshot.clone(),
                lambda_runtime::LambdaEvent::new(payload, context),
            )
            .await
            .unwrap();
            let encoded = serde_json::to_vec(&reply).unwrap();
            let reparsed: Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(reparsed, reply, "{id}: ALB envelope JSON");
            assert_eq!(reply["statusCode"], case["expectedStatus"], "{id}");
            assert_eq!(
                reply["body"], case["proposedBody"],
                "{id}: proposed bytes qualification only"
            );
            if case["expectedStatus"] == 400 {
                let actual: Value = serde_json::from_str(reply["body"].as_str().unwrap()).unwrap();
                let expected: Value =
                    serde_json::from_str(case["proposedBody"].as_str().unwrap()).unwrap();
                assert_eq!(
                    actual["description"], expected["description"],
                    "{id}: decoded description"
                );
                assert_eq!(actual["severity"], "ERROR");
            }
        }
        assert_eq!(
            counts.dispatches.load(Ordering::SeqCst),
            case["expectedDispatches"].as_u64().unwrap() as usize * 2,
            "{id}"
        );
        assert_eq!(counts.completions.load(Ordering::SeqCst), 2, "{id}");
        drop(snapshot);
        child.lock().unwrap().take();
    }
}

use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use gateway_policy::openapi_operation::{BasePathProfile, OperationCatalogue, SourceFormat};
use light_lambda_runtime::{
    init::*, init_error::InitError, specification_policy::SpecificationPolicyProvider,
};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde_json::{Value, json};
use serverless_aws::target::GatewayIdentity;
use serverless_core::{
    compat::COMPATIBILITY, handlers::specification::SpecificationHandler, plan::*, *,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test-only\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 50\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /x@get: TestFn\nregion: us-east-1\n",
    },
];

#[tokio::test]
async fn opt_in_metadata_keeps_original_numeric_source_and_legacy_parser() {
    for name in ["openapi.yml", "openapi.yaml", "openapi.json", "spec.yaml"] {
        let dir = tempfile::tempdir().unwrap();
        files(dir.path());
        std::fs::write(dir.path().join("server.yml"), EMBEDDED[0].content).unwrap();
        let raw = if name.ends_with(".json") {
            r#"{"openapi":"3.1.0","paths":{"/x":{"get":{}}},"components":{"schemas":{"Huge":{"maximum":1e400}}}}"#
        } else {
            "openapi: 3.1.0\npaths: {'/x': {get: {}}}\ncomponents: {schemas: {Huge: {maximum: 1e400}}}\n"
        };
        std::fs::write(dir.path().join(name), raw).unwrap();
        let config = light_runtime::LightRuntimeBuilder::new(ConfigPreparationTransport)
            .with_config_dir(dir.path())
            .with_external_config_dir(dir.path())
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        if name.ends_with(".json") {
            assert!(
                SpecificationPolicyProvider
                    .prepare_specification_resources(&config, None)
                    .is_err()
            );
        }
        let captured = Mutex::new(None);
        let prepared = SpecificationPolicyProvider
            .prepare_with_metadata_projection(&config, |source, format| {
                assert_eq!(&*source, raw.as_bytes());
                assert_eq!(matches!(format, SourceFormat::Json), name.ends_with(".json"));
                *captured.lock().unwrap() = Some(source);
                std::fs::write(dir.path().join(name), "invalid: [").unwrap();
                Ok(br#"{"openapi":"3.1.0","paths":{"/x":{"get":{}}},"components":{"schemas":{"Huge":{"maximum":0}}}}"#.to_vec())
            })
            .unwrap();
        let resources = prepared.resources.unwrap();
        let (_, retained) = resources.catalogue.source_resource().unwrap();
        assert!(Arc::ptr_eq(
            &retained,
            captured.lock().unwrap().as_ref().unwrap()
        ));
        assert_eq!(&*retained, raw.as_bytes());
        assert!(resources.catalogue.operation("/x", "GET").is_some());
        let mut hash = sha2::Sha256::new();
        use sha2::Digest;
        hash.update(b"light-lambda/specification-source/v1\0");
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((raw.len() as u64).to_be_bytes());
        hash.update(raw.as_bytes());
        assert_eq!(prepared.prepared.1, Some(hash.finalize().into()));
        assert!(
            SpecificationPolicyProvider
                .prepare_specification_resources(&config, None)
                .is_err()
        );
    }
}

struct OwnedPrecisionChild(std::process::Child);

#[tokio::test]
async fn projected_json_encoding_does_not_narrow_admitted_yaml_tree() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path());
    std::fs::write(dir.path().join("server.yml"), EMBEDDED[0].content).unwrap();
    let text = "\t".repeat(600_000);
    let raw = format!("openapi: 3.1.0\npaths: {{'/x': {{get: {{}}}}}}\ndescription: \"{text}\"\n");
    assert!(raw.len() < gateway_policy::openapi_operation::MAX_SOURCE_BYTES);
    std::fs::write(dir.path().join("openapi.yml"), &raw).unwrap();
    let config = light_runtime::LightRuntimeBuilder::new(ConfigPreparationTransport)
        .with_config_dir(dir.path())
        .with_external_config_dir(dir.path())
        .build()
        .prepare_local_config()
        .await
        .unwrap();
    let ordinary = SpecificationPolicyProvider
        .prepare_specification_resources(&config, None)
        .unwrap();
    let projected = SpecificationPolicyProvider
        .prepare_with_metadata_projection(&config, |capture, _| {
            assert_eq!(&*capture, raw.as_bytes());
            let value: Value = serde_yaml::from_slice(&capture).unwrap();
            let json = serde_json::to_vec(&value).unwrap();
            assert!(json.len() > gateway_policy::openapi_operation::MAX_SOURCE_BYTES);
            Ok(json)
        })
        .unwrap();
    assert_eq!(ordinary.prepared.1, projected.prepared.1);
    assert!(
        projected
            .resources
            .unwrap()
            .catalogue
            .operation("/x", "GET")
            .is_some()
    );
}

impl OwnedPrecisionChild {
    fn exchange(&mut self, value: &Value) -> Value {
        use std::io::{Read, Write};
        let bytes = serde_json::to_vec(value).unwrap();
        assert!(bytes.len() <= 4 * 1024 * 1024);
        let input = self.0.stdin.as_mut().unwrap();
        input
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
        let output = self.0.stdout.as_mut().unwrap();
        let mut header = [0; 4];
        output.read_exact(&mut header).unwrap();
        let count = u32::from_be_bytes(header) as usize;
        assert!(count <= 4 * 1024 * 1024);
        let mut bytes = vec![0; count];
        output.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
impl Drop for OwnedPrecisionChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().unwrap();
    }
}

struct PrecisionProducer {
    binary: std::path::PathBuf,
    child: Arc<Mutex<Option<OwnedPrecisionChild>>>,
    captured: Arc<Mutex<Option<Arc<[u8]>>>>,
    selected: std::path::PathBuf,
}
impl HandlerFactoryProvider for PrecisionProducer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("dedicated resource entry required")
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        SpecificationPolicyProvider.prepare_with_metadata_projection(config, |source, format| {
            use sha2::{Digest, Sha256};
            let process = std::process::Command::new(&self.binary)
                .env_clear()
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn().unwrap();
            let mut child = OwnedPrecisionChild(process);
            let ready = child.exchange(&json!({"source": std::str::from_utf8(&source).unwrap(), "format": if matches!(format, SourceFormat::Json) { "json" } else { "yaml" }}));
            assert_eq!(ready["ready"], true);
            assert_eq!(ready["sourceSha256"], format!("{:x}", Sha256::digest(&source)));
            assert_eq!(ready["requestSchemaCount"], 1);
            assert_eq!(ready["roots"].as_array().unwrap().len(), 2);
            *self.captured.lock().unwrap() = Some(source);
            std::fs::write(&self.selected, "changed after capture: [").unwrap();
            let metadata = ready["metadataJson"].as_str().unwrap().as_bytes().to_vec();
            *self.child.lock().unwrap() = Some(child);
            Ok(metadata)
        })
    }
}

struct VocabularyProducer(PrecisionProducer, usize, usize);
impl HandlerFactoryProvider for VocabularyProducer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("resource path required")
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        SpecificationPolicyProvider.prepare_with_metadata_projection(config,|source,format| {
            use std::io::{Read,Write};
            use sha2::{Digest,Sha256};
            let invalid=||InitError::InvalidConfiguration("owned vocabulary: invalid captured catalogue");
            let process=std::process::Command::new(&self.0.binary).env_clear().stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().map_err(|_|invalid())?;
            let mut child=OwnedPrecisionChild(process);
            let bytes=serde_json::to_vec(&json!({"protocol":"wp12-owned-request-v2","source":std::str::from_utf8(&source).unwrap(),"format":if matches!(format,SourceFormat::Json){"json"}else{"yaml"}})).unwrap();
            assert!(bytes.len()<=4*1024*1024);
            let input=child.0.stdin.as_mut().unwrap();
            input.write_all(&(bytes.len()as u32).to_be_bytes()).map_err(|_|invalid())?;
            input.write_all(&bytes).map_err(|_|invalid())?;input.flush().map_err(|_|invalid())?;
            let output=child.0.stdout.as_mut().unwrap();
            let mut header=[0;4];output.read_exact(&mut header).map_err(|_|invalid())?;
            let count=u32::from_be_bytes(header)as usize;if count>4*1024*1024{return Err(invalid())}
            let mut bytes=vec![0;count];output.read_exact(&mut bytes).map_err(|_|invalid())?;
            let ready:Value=serde_json::from_slice(&bytes).map_err(|_|invalid())?;
            assert_eq!(ready["ready"],true);assert_eq!(ready["sourceSha256"],format!("{:x}",Sha256::digest(&source)));
            assert_eq!(ready["requestSchemaCount"],self.2);assert_eq!(ready["roots"].as_array().unwrap().len(),self.1);
            *self.0.captured.lock().unwrap()=Some(source);
            std::fs::write(&self.0.selected,"changed after capture: [").unwrap();
            let metadata=ready["metadataJson"].as_str().unwrap().as_bytes().to_vec();
            *self.0.child.lock().unwrap()=Some(child);Ok(metadata)
        })
    }
}

struct PrecisionConsumer(Arc<Mutex<Option<Arc<[u8]>>>>);
impl HandlerFactoryProvider for PrecisionConsumer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("dedicated Validator preparation required")
    }
    fn prepare_validator(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let resources = resources.unwrap();
        let (_, raw) = resources.catalogue.source_resource().unwrap();
        assert!(Arc::ptr_eq(&raw, self.0.lock().unwrap().as_ref().unwrap()));
        assert!(resources.catalogue.operation("/x", "POST").is_some());
        Ok(Arc::new(ProbeFactory))
    }
}

#[tokio::test]
#[ignore = "owned isolated precision worker; requires WP12_PRECISION_BINARY and WP12_NUMERIC_ORACLE_DIR"]
async fn complete_precision_child_uses_real_capture_and_common_init_bridge() {
    let binary = std::path::PathBuf::from(std::env::var_os("WP12_PRECISION_BINARY").unwrap());
    let oracle = std::path::PathBuf::from(std::env::var_os("WP12_NUMERIC_ORACLE_DIR").unwrap());
    let amendments: Value = serde_json::from_str(include_str!("../../../tests/serverless-conformance/amendments/wp12-validator-numeric-compatibility-v1.json")).unwrap();
    for case in amendments["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let raw = std::fs::read(oracle.join(format!("{id}.source"))).unwrap();
        for name in if id.starts_with("yaml") {
            &["openapi.yml", "openapi.yaml", "spec.yaml"][..]
        } else {
            &["openapi.json"][..]
        } {
            let dir = tempfile::tempdir().unwrap();
            files(dir.path());
            let selected = dir.path().join(name);
            std::fs::write(&selected, &raw).unwrap();
            let child = Arc::new(Mutex::new(None));
            let captured = Arc::new(Mutex::new(None));
            let entries = vec![
                HandlerRegistration {
                    id: HandlerId::Validator,
                    config_file: "openapi-validator.yml",
                    factory: Some(Arc::new(PrecisionConsumer(captured.clone()))),
                },
                HandlerRegistration {
                    id: HandlerId::Specification,
                    config_file: "openapi-handler.yml",
                    factory: Some(Arc::new(PrecisionProducer {
                        binary: binary.clone(),
                        child: child.clone(),
                        captured: captured.clone(),
                        selected,
                    })),
                },
            ];
            let _snapshot = initialize(input(dir.path(), entries, false)).await.unwrap();
            assert_eq!(
                &**captured.lock().unwrap().as_ref().unwrap(),
                raw.as_slice()
            );
            let reply = child.lock().unwrap().as_mut().unwrap().exchange(&json!({"event": {"path":"/x", "httpMethod":"POST", "headers":{"Content-Type":"application/json"}, "body":case["requestBody"]}}));
            assert_eq!(reply["status"], case["status"], "{id}/{name}");
            assert_eq!(reply["body"], case["body"], "{id}/{name}");
            child.lock().unwrap().take();
        }
    }
}
const JSON: &str = r#"{"openapi":"3.1.0","components":{"securitySchemes":{"z":{"type":"oauth2","flows":{}},"a":{"type":"oauth2","flows":{}}}},"paths":{"/x":{"get":{"operationId":"captured","security":[{"z":["write"],"a":["read"]}]}}}}"#;
const YAML: &str = "openapi: 3.0.3\ncomponents:\n  securitySchemes:\n    a: {type: oauth2, flows: {}}\n    z: {type: oauth2, flows: {}}\npaths:\n  /x:\n    get:\n      operationId: captured\n      security: [{a: [read], z: [write]}]\n";

fn files(dir: &std::path::Path) {
    for (file, text) in [
        ("openapi-handler.yml", "enabled: true\n"),
        ("openapi-validator.yml", "enabled: true\n"),
    ] {
        std::fs::write(dir.join(file), text).unwrap();
    }
    let declarations: Vec<_> = [
        (HandlerId::Specification, "spec"),
        (HandlerId::Validator, "validator"),
        (HandlerId::Proxy, "proxy"),
    ]
    .into_iter()
    .map(|(id, alias)| {
        format!(
            "{}@{alias}",
            COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class
        )
    })
    .collect();
    std::fs::write(dir.join("handler.yml"), serde_yaml::to_string(&json!({"enabled":true,"handlers":declarations,"chains":{"nested":["spec","validator","spec","validator"]},"defaultHandlers":["nested","proxy"]})).unwrap()).unwrap();
}
fn input(dir: &std::path::Path, handlers: Vec<HandlerRegistration>, offline: bool) -> InitInput {
    InitInput {
        embedded: EMBEDDED,
        local_dir: dir.join("absent"),
        default_dir: None,
        external_dir: dir.into(),
        environment: Environment {
            region: Some("us-east-1".into()),
            function_name: "test-only-gateway".into(),
            function_version: "1".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: false,
        },
        gateway_identity: GatewayIdentity {
            function_name: "test-only-gateway".into(),
            region: "us-east-1".into(),
            account_id: "000000000000".into(),
        },
        handlers,
        finalizers: vec![],
        backend: if offline {
            BackendConfiguration::Offline
        } else {
            BackendConfiguration::Injected(Arc::new(NoDispatch))
        },
    }
}
struct NoDispatch;
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        panic!("Init must not dispatch")
    }
}
struct Producer {
    calls: Arc<AtomicUsize>,
    omit_resources: bool,
}
impl HandlerFactoryProvider for Producer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("legacy create must not be called by dedicated preparation")
    }
    // Preserve the supported legacy absence of extra resources in this probe.
    fn prepare_specification(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationFactory, InitError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        SpecificationPolicyProvider.prepare_specification(config, security)
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        if self.omit_resources {
            return Ok(PreparedSpecificationResources {
                prepared: self.prepare_specification(config, security)?,
                resources: None,
            });
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        SpecificationPolicyProvider.prepare_specification_resources(config, security)
    }
}
struct Probe {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Option<Arc<SpecificationResources>>>>,
    expected: &'static [&'static str],
}
impl HandlerFactoryProvider for Probe {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("dedicated Validator entry required")
    }
    fn prepare_validator(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let resources =
            resources.ok_or(InitError::InvalidConfiguration("test: missing resources"))?;
        let operation =
            resources
                .catalogue
                .operation("/x", "get")
                .ok_or(InitError::InvalidConfiguration(
                    "test: mismatched resources",
                ))?;
        if operation
            .ordered_oauth2_names()
            .map(|names| names.iter().map(String::as_str).collect::<Vec<_>>())
            != Some(self.expected.to_vec())
        {
            return Err(InitError::InvalidConfiguration(
                "test: mismatched resources",
            ));
        }
        *self.seen.lock().unwrap() = Some(resources);
        Ok(Arc::new(ProbeFactory))
    }
}
struct ProbeFactory;
impl HandlerFactory for ProbeFactory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        Ok(Binding::Request(
            Arc::new(ProbeFactory),
            RequestCapabilities::default(),
        ))
    }
}
#[async_trait]
impl RequestHandler for ProbeFactory {
    fn id(&self) -> HandlerId {
        HandlerId::Validator
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        panic!("resource probe must not execute")
    }
}
fn registrations(probe: Arc<Probe>, reversed: bool) -> Vec<HandlerRegistration> {
    let mut entries = vec![
        HandlerRegistration {
            id: HandlerId::Specification,
            config_file: "openapi-handler.yml",
            factory: Some(Arc::new(SpecificationPolicyProvider)),
        },
        HandlerRegistration {
            id: HandlerId::Validator,
            config_file: "openapi-validator.yml",
            factory: Some(probe),
        },
    ];
    if reversed {
        entries.reverse();
    }
    entries
}

#[tokio::test]
async fn shuffled_nested_consumers_share_ordered_resources_offline_and_startup() {
    for (name, source, expected) in [
        ("openapi.json", JSON, &["z", "a"][..]),
        ("openapi.yml", YAML, &["a", "z"][..]),
    ] {
        for reversed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            files(dir.path());
            std::fs::write(dir.path().join(name), source).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::new(Mutex::new(None));
            let probe = Arc::new(Probe {
                calls: calls.clone(),
                seen: seen.clone(),
                expected,
            });
            let report = validate_configuration(&input(
                dir.path(),
                registrations(probe.clone(), reversed),
                true,
            ))
            .await;
            assert!(report.valid, "{:?}", report.errors);
            let first = seen.lock().unwrap().clone().unwrap();
            let snapshot = initialize(input(dir.path(), registrations(probe, reversed), false))
                .await
                .unwrap();
            assert_eq!(
                report.config_digest.as_deref(),
                Some(snapshot.config_digest())
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2); // once per Init, not per repeated occurrence
            let second = seen.lock().unwrap().clone().unwrap();
            assert!(!Arc::ptr_eq(&first, &second)); // never memoize across Init
        }
    }
}

#[tokio::test]
async fn validation_precedes_capture_and_missing_or_mismatched_resources_fail_closed() {
    for mode in [
        "absent",
        "mismatch",
        "duplicate",
        "wrong-config",
        "proxy",
        "missing-validator",
        "missing-specification",
    ] {
        let dir = tempfile::tempdir().unwrap();
        files(dir.path());
        std::fs::write(dir.path().join("openapi.json"), JSON).unwrap();
        let producer_calls = Arc::new(AtomicUsize::new(0));
        let consumer_calls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::new(Probe {
            calls: consumer_calls.clone(),
            seen: Arc::new(Mutex::new(None)),
            expected: if mode == "mismatch" {
                &["a", "z"]
            } else {
                &["z", "a"]
            },
        });
        let mut entries = registrations(probe, true);
        entries[1].factory = Some(Arc::new(Producer {
            calls: producer_calls.clone(),
            omit_resources: mode == "absent",
        }));
        match mode {
            "duplicate" => entries.push(HandlerRegistration {
                id: HandlerId::Specification,
                config_file: "openapi-handler.yml",
                factory: None,
            }),
            "wrong-config" => entries.push(HandlerRegistration {
                id: HandlerId::Specification,
                config_file: "openapi-validator.yml",
                factory: None,
            }),
            "proxy" => entries.push(HandlerRegistration {
                id: HandlerId::Proxy,
                config_file: "openapi-handler.yml",
                factory: None,
            }),
            "missing-validator" => entries[0].factory = None,
            "missing-specification" => entries[1].factory = None,
            _ => {}
        }
        let report = validate_configuration(&input(dir.path(), entries, true)).await;
        assert!(!report.valid, "{mode}");
        let reaches_consumer = matches!(mode, "absent" | "mismatch");
        assert_eq!(
            producer_calls.load(Ordering::SeqCst),
            usize::from(reaches_consumer),
            "{mode}"
        );
        assert_eq!(
            consumer_calls.load(Ordering::SeqCst),
            usize::from(reaches_consumer),
            "{mode}"
        );
        assert!(!format!("{:?}", report.errors).contains("captured"));
    }
}

fn invocation() -> Invocation {
    Invocation {
        method: http::Method::GET,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers: http::HeaderMap::new(),
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        response_deadline: tokio::time::Instant::now() + Duration::from_secs(3),
        cancel: tokio_util::sync::CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: Value::Null,
    }
}

// Owned integration only: synchronous child I/O is deliberately not a
// production executor, deadline controller, or registered app provider.

struct VocabularyRuntimeConsumer(Arc<Mutex<Option<OwnedPrecisionChild>>>);
impl HandlerFactoryProvider for VocabularyRuntimeConsumer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("resource path required")
    }
    fn prepare_validator(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        assert!(resources.unwrap().catalogue.source_resource().is_some());
        Ok(Arc::new(Self(self.0.clone())))
    }
}
impl HandlerFactory for VocabularyRuntimeConsumer {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        ValidatorMediaRole::Requires
    }
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        Ok(Binding::Request(
            Arc::new(Self(self.0.clone())),
            RequestCapabilities::default(),
        ))
    }
}
#[async_trait]
impl RequestHandler for VocabularyRuntimeConsumer {
    fn id(&self) -> HandlerId {
        HandlerId::Validator
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        OperationAuthorizationRole::MetadataConsumer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        assert!(state.operation.is_some());
        let media = if state.validator_media_is_json()? {
            "application/json"
        } else {
            "text/plain"
        };
        let body = inv
            .body
            .as_ref()
            .map(|b| std::str::from_utf8(b).map(str::to_owned))
            .transpose()
            .map_err(|_| HandlerFailure)?;
        let reply=self.0.lock().unwrap().as_mut().unwrap().exchange(&json!({"event":{"path":inv.path,"httpMethod":inv.method.as_str(),"headers":{"Content-Type":media},"body":body}}));
        match reply["status"].as_u64() {
            Some(200) => Ok(HandlerOutcome::Continue),
            Some(400) => {
                let violation = &reply["violation"];
                // Owned Validator-local rendering qualification. The shared
                // Status renderer deliberately remains unchanged.
                let body = reply["body"].as_str().ok_or(HandlerFailure)?;
                let parsed: Value = serde_json::from_str(body).map_err(|_| HandlerFailure)?;
                assert_eq!(parsed["description"], violation["description"]);
                assert_eq!(parsed["code"], violation["code"]);
                assert_eq!(parsed["message"], violation["message"]);
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                Ok(HandlerOutcome::Respond(GatewayResponse {
                    status: http::StatusCode::BAD_REQUEST,
                    headers,
                    body: Some(bytes::Bytes::copy_from_slice(body.as_bytes())),
                    body_is_binary: false,
                }))
            }
            _ => Err(HandlerFailure),
        }
    }
}

struct PrecisionRuntimeConsumer(Arc<Mutex<Option<OwnedPrecisionChild>>>);
impl HandlerFactoryProvider for PrecisionRuntimeConsumer {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        panic!("dedicated resource preparation required")
    }
    fn prepare_validator(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        assert!(resources.unwrap().catalogue.source_resource().is_some());
        Ok(Arc::new(Self(self.0.clone())))
    }
}
impl HandlerFactory for PrecisionRuntimeConsumer {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        ValidatorMediaRole::Requires
    }
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        Ok(Binding::Request(
            Arc::new(Self(self.0.clone())),
            RequestCapabilities::default(),
        ))
    }
}
#[async_trait]
impl RequestHandler for PrecisionRuntimeConsumer {
    fn id(&self) -> HandlerId {
        HandlerId::Validator
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        OperationAuthorizationRole::MetadataConsumer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        assert!(state.operation.is_some());
        let media = if state.validator_media_is_json()? {
            "application/json"
        } else {
            "text/plain"
        };
        let body = inv
            .body
            .as_ref()
            .map(|b| std::str::from_utf8(b).map(str::to_owned))
            .transpose()
            .map_err(|_| HandlerFailure)?;
        let reply = self.0.lock().unwrap().as_mut().unwrap().exchange(&json!({"event":{"path":inv.path,"httpMethod":inv.method.as_str(),"headers":{"Content-Type":media},"body":body}}));
        match reply["status"].as_u64() {
            Some(200) => Ok(HandlerOutcome::Continue),
            Some(400) => {
                let body: Value =
                    serde_json::from_str(reply["body"].as_str().ok_or(HandlerFailure)?)
                        .map_err(|_| HandlerFailure)?;
                let error = StatusError::new(
                    400,
                    body["code"].as_str().ok_or(HandlerFailure)?,
                    body["message"].as_str().ok_or(HandlerFailure)?,
                    body["description"].as_str().ok_or(HandlerFailure)?,
                );
                assert_eq!(
                    error.to_bytes().as_ref(),
                    reply["body"].as_str().unwrap().as_bytes()
                );
                Ok(HandlerOutcome::Reject(error))
            }
            _ => Err(HandlerFailure),
        }
    }
}
#[derive(Default)]
struct PrecisionDispatchCounts {
    dispatches: AtomicUsize,
    completions: AtomicUsize,
}
#[async_trait]
impl Backend for PrecisionDispatchCounts {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
impl Finalizer for PrecisionDispatchCounts {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, _: &GatewayResponse) {
        assert!(state.metrics.finalized);
        self.completions.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
#[ignore = "owned precision composition; requires WP12_PRECISION_BINARY and WP12_NUMERIC_ORACLE_DIR"]
async fn numeric_amendments_through_real_service_have_exact_bytes_and_zero_or_one_dispatch() {
    let binary = std::path::PathBuf::from(std::env::var_os("WP12_PRECISION_BINARY").unwrap());
    let oracle = std::path::PathBuf::from(std::env::var_os("WP12_NUMERIC_ORACLE_DIR").unwrap());
    let amendments: Value = serde_json::from_str(include_str!("../../../tests/serverless-conformance/amendments/wp12-validator-numeric-compatibility-v1.json")).unwrap();
    for case in amendments["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let raw = std::fs::read(oracle.join(format!("{id}.source"))).unwrap();
        let dir = tempfile::tempdir().unwrap();
        files(dir.path());
        std::fs::write(
            dir.path().join("lambda-proxy.yml"),
            "functions:\n  /x@post: TestFn\nregion: us-east-1\n",
        )
        .unwrap();
        let selected = dir.path().join(if id.starts_with("yaml") {
            "openapi.yml"
        } else {
            "openapi.json"
        });
        std::fs::write(&selected, raw).unwrap();
        let child = Arc::new(Mutex::new(None));
        let entries = vec![
            HandlerRegistration {
                id: HandlerId::Validator,
                config_file: "openapi-validator.yml",
                factory: Some(Arc::new(PrecisionRuntimeConsumer(child.clone()))),
            },
            HandlerRegistration {
                id: HandlerId::Specification,
                config_file: "openapi-handler.yml",
                factory: Some(Arc::new(PrecisionProducer {
                    binary: binary.clone(),
                    child: child.clone(),
                    captured: Arc::new(Mutex::new(None)),
                    selected,
                })),
            },
        ];
        let counts = Arc::new(PrecisionDispatchCounts::default());
        let mut init = input(dir.path(), entries, false);
        init.backend = BackendConfiguration::Injected(counts.clone());
        init.finalizers = vec![counts.clone()];
        let snapshot = initialize(init).await.unwrap();
        for _ in 0..2 {
            let payload = json!({"path":"/x","httpMethod":"POST","multiValueHeaders":{"Content-Type":["application/json"]},"body":case["requestBody"],"isBase64Encoded":false,"requestContext":{"elb":{"targetGroupArn":"arn:aws:elasticloadbalancing:us-east-1:000000000000:targetgroup/test-only/0000000000000000"}}});
            let mut context = lambda_runtime::Context::default();
            context.request_id = "owned-precision".into();
            context.deadline = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 5000;
            let reply = light_lambda_runtime::service::handle(
                snapshot.clone(),
                lambda_runtime::LambdaEvent::new(payload, context),
            )
            .await
            .unwrap();
            assert_eq!(reply["statusCode"], case["status"], "{id}");
            assert_eq!(reply["body"], case["body"], "{id}");
        }
        assert_eq!(
            counts.dispatches.load(Ordering::SeqCst),
            case["dispatches"].as_u64().unwrap() as usize * 2,
            "{id}"
        );
        assert_eq!(counts.completions.load(Ordering::SeqCst), 2, "{id}");
        child.lock().unwrap().take();
    }
}
#[tokio::test]
async fn same_capture_factory_shares_descriptor_arcs_and_survives_file_mutation() {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path());
    std::fs::write(dir.path().join("server.yml"), EMBEDDED[0].content).unwrap();
    std::fs::write(dir.path().join("openapi.json"), JSON).unwrap();
    let config = light_runtime::LightRuntimeBuilder::new(
        light_lambda_runtime::init::ConfigPreparationTransport,
    )
    .with_config_dir(dir.path())
    .with_external_config_dir(dir.path())
    .build()
    .prepare_local_config()
    .await
    .unwrap();
    let result = SpecificationPolicyProvider
        .prepare_specification_resources(&config, None)
        .unwrap();
    let resources = result.resources.unwrap();
    let operation = resources.catalogue.operation("/x", "get").unwrap();
    std::fs::write(
        dir.path().join("openapi.json"),
        JSON.replace("captured", "changed"),
    )
    .unwrap();
    let compatibility = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Specification)
        .unwrap();
    let Binding::Request(handler, _) = result.prepared.0.build(compatibility, "spec").unwrap()
    else {
        panic!("request binding")
    };
    let mut state = ExchangeState::default();
    assert!(matches!(
        handler
            .on_request(&mut invocation(), &mut state)
            .await
            .unwrap(),
        HandlerOutcome::Continue
    ));
    assert!(Arc::ptr_eq(state.operation.as_ref().unwrap(), &operation));
    assert_eq!(operation.operation_id(), Some("captured"));
    let next = SpecificationPolicyProvider
        .prepare_specification_resources(&config, None)
        .unwrap();
    assert_ne!(result.prepared.1, next.prepared.1);
    assert_eq!(
        next.resources
            .unwrap()
            .catalogue
            .operation("/x", "get")
            .unwrap()
            .operation_id(),
        Some("changed")
    );
    let value: Value = serde_json::from_str(JSON).unwrap();
    let catalogue = OperationCatalogue::from_document(&value).unwrap();
    let unknown = catalogue.operation("/x", "get").unwrap();
    let profile =
        Arc::new(BasePathProfile::from_source(JSON.as_bytes(), SourceFormat::Json, None).unwrap());
    let metadata_only = SpecificationHandler::from_prepared(catalogue, profile).unwrap();
    let mut state = ExchangeState::default();
    metadata_only
        .on_request(&mut invocation(), &mut state)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(state.operation.as_ref().unwrap(), &unknown));
    assert!(state.operation.unwrap().ordered_oauth2_names().is_none());
}
