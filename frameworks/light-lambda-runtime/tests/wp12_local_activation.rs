//! Activation/preparation qualification using synthetic terminal factories.
//! This does not qualify real admin response/authentication behavior.
use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use light_lambda_runtime::init::*;
use light_lambda_runtime::init_error::InitError;
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde_json::json;
use serverless_aws::target::GatewayIdentity;
use serverless_core::{compat::COMPATIBILITY, plan::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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
struct Probe(HandlerId, Arc<AtomicUsize>);
impl HandlerFactoryProvider for Probe {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Terminal(self.0)))
    }
}
struct Terminal(HandlerId);
impl HandlerFactory for Terminal {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        Ok(Binding::Local(Arc::new(Terminal(self.0))))
    }
}
#[async_trait]
impl RequestHandler for Terminal {
    fn id(&self) -> HandlerId {
        self.0
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        panic!("preparation probe must never execute")
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
        panic!("preparation must never dispatch")
    }
}
fn input(dir: &std::path::Path, counter: Arc<AtomicUsize>, offline: bool) -> InitInput {
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
        handlers: [HandlerId::Info, HandlerId::GetLogger, HandlerId::SetLogger]
            .into_iter()
            .map(|id| HandlerRegistration {
                id,
                config_file: if id == HandlerId::Info {
                    "info.yml"
                } else {
                    "logging.yml"
                },
                factory: (id != HandlerId::SetLogger).then(|| {
                    Arc::new(Probe(id, counter.clone())) as Arc<dyn HandlerFactoryProvider>
                }),
            })
            .collect(),
        finalizers: vec![],
        backend: if offline {
            BackendConfiguration::Offline
        } else {
            BackendConfiguration::Injected(Arc::new(NoDispatch))
        },
    }
}
fn files(dir: &std::path::Path, info: &str, logging: &str, setter: bool) {
    std::fs::write(dir.join("info.yml"), info).unwrap();
    std::fs::write(dir.join("logging.yml"), logging).unwrap();
    let mappings: Vec<_> = [
        HandlerId::Proxy,
        HandlerId::Info,
        HandlerId::GetLogger,
        HandlerId::SetLogger,
    ]
    .into_iter()
    .map(|id| {
        let class = COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
        format!("{class}@{id:?}")
    })
    .collect();
    std::fs::write(dir.join("handler.yml"), serde_yaml::to_string(&json!({"enabled":true,"handlers":mappings,"defaultHandlers":if setter { vec!["SetLogger", "Proxy"] } else { vec!["Proxy"] }})).unwrap()).unwrap();
}
async fn consistent(info: &str, logging: &str, setter: bool, expected: Option<usize>) {
    let dir = tempfile::tempdir().unwrap();
    files(dir.path(), info, logging, setter);
    let offline_count = Arc::new(AtomicUsize::new(0));
    let startup_count = Arc::new(AtomicUsize::new(0));
    let report = validate_configuration(&input(dir.path(), offline_count.clone(), true)).await;
    let startup = initialize(input(dir.path(), startup_count.clone(), false)).await;
    match expected {
        Some(count) => {
            assert!(report.valid, "{:?}", report.errors);
            let startup = startup.unwrap();
            assert_eq!(offline_count.load(Ordering::SeqCst), count);
            assert_eq!(startup_count.load(Ordering::SeqCst), count);
            assert_eq!(
                report.config_digest.as_deref(),
                Some(startup.config_digest())
            );
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.starts_with("info.downstreamEnabled:"))
            );
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.starts_with("logger.downstreamEnabled:"))
            );
        }
        None => {
            assert!(!report.valid);
            assert!(startup.is_err());
            assert_eq!(
                report.errors,
                vec![startup.err().unwrap().message().to_string()]
            );
        }
    }
}
#[tokio::test]
async fn info_activation_is_explicit_and_consistent() {
    consistent(
        "enableServerInfo: true\n",
        "enabled: false\n",
        false,
        Some(1),
    )
    .await;
    consistent(
        "enableServerInfo: false\n",
        "enabled: false\n",
        false,
        Some(0),
    )
    .await;
    consistent("{}\n", "{}\n", false, Some(0)).await;
}
#[tokio::test]
async fn logger_get_does_not_activate_set_catalog_entry() {
    consistent(
        "enableServerInfo: false\n",
        "enabled: true\n",
        false,
        Some(1),
    )
    .await;
}
#[tokio::test]
async fn executable_set_selection_is_rejected_even_when_logging_disabled() {
    for logging in ["enabled: true\n", "enabled: false\n"] {
        consistent("enableServerInfo: false\n", logging, true, None).await;
    }
}
#[tokio::test]
async fn malformed_and_unsupported_configuration_fails_both_paths() {
    for info in [
        "enableServerInfo: yes\n",
        "enableServerInfo: null\n",
        "enabled: true\n",
        "enableServerInfo: false\ndownstreamEnabled: true\n",
        "enableServerInfo: false\nkeysToNotSort: []\n",
        "[true]\n",
    ] {
        consistent(info, "enabled: false\n", false, None).await;
    }
    for logging in [
        "enabled: null\n",
        "enabled: 1\n",
        "enabled: true\nsetEnabled: true\n",
        "enabled: false\ndownstreamEnabled: true\n",
        "enabled: false\nlogStart: 1\n",
        "[false]\n",
    ] {
        consistent("enableServerInfo: false\n", logging, false, None).await;
    }
}
