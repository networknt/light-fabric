use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serde_json::json;
struct NoDispatch;
#[async_trait::async_trait]
impl serverless_core::Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &serverless_core::Invocation,
        _: &serverless_core::ExchangeState,
        _: &serverless_core::Target,
    ) -> Result<serverless_core::GatewayResponse, serverless_core::DispatchError> {
        panic!("Init must never dispatch")
    }
}
#[tokio::test]
async fn selected_dynamo_is_rejected_by_real_and_offline_init_without_network() {
    for (service, rejected) in [
        (json!({}), false),
        (
            json!({"singletons":[{"other.Service":["com.networknt.aws.lambda.cache.DynamoDbCacheManager"]}]}),
            false,
        ),
        (
            json!({"singletons":[{"com.networknt.cache.CacheManager":[]}]}),
            false,
        ),
        (
            json!({"singletons":[{"com.networknt.cache.CacheManager":["unrelated.Manager"]}]}),
            false,
        ),
        (
            json!({"singletons":[{"com.networknt.cache.CacheManager":["unrelated.Manager","com.networknt.aws.lambda.cache.DynamoDbCacheManager"]}]}),
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("service.yml"), service.to_string()).unwrap();
        std::fs::write(dir.path().join("values.yml"),"light-lambda.stage: test-only\nsecurity.enabled: false\nunified-security.enabled: false\n").unwrap();
        let mut input = light_lambda_runtime::init::InitInput {
            embedded: &[
                config_loader::EmbeddedConfigFile {
                    name: "server.yml",
                    content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
                },
                config_loader::EmbeddedConfigFile {
                    name: "light-lambda.yml",
                    content: "stage: test-only\ningressTimeoutMs: 2000\nresponseReserveMs: 200\npostReserveMs: 50\n",
                },
                config_loader::EmbeddedConfigFile {
                    name: "handler.yml",
                    content: "handlers: [com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy]\ndefaultHandlers: [proxy]\n",
                },
                config_loader::EmbeddedConfigFile {
                    name: "lambda-proxy.yml",
                    content: "functions: { /x@get: TestFn }\nregion: us-east-1\n",
                },
            ],
            local_dir: dir.path().join("absent"),
            default_dir: None,
            external_dir: dir.path().into(),
            environment: light_lambda_runtime::init::Environment {
                region: Some("us-east-1".into()),
                function_name: "test-only".into(),
                function_version: "1".into(),
                memory_mb: 128,
                allow_insecure_test_endpoints: false,
            },
            gateway_identity: serverless_aws::target::GatewayIdentity {
                function_name: "test-only".into(),
                region: "us-east-1".into(),
                account_id: "000000000000".into(),
            },
            handlers: vec![],
            finalizers: vec![],
            backend: BackendConfiguration::Offline,
        };
        let offline = validate_configuration(&input).await;
        input.backend = BackendConfiguration::Injected(std::sync::Arc::new(NoDispatch));
        let real = initialize(input).await;
        if rejected {
            let error = real.err().unwrap();
            assert_eq!(
                error.message(),
                "service.yml: DynamoDbCacheManager unavailable"
            );
            assert!(!offline.valid);
            assert_eq!(offline.errors, vec![error.message().to_string()]);
        } else {
            assert!(offline.valid, "{:?}", offline.errors);
            assert!(real.is_ok(), "{:?}", real.err());
        }
    }
}
