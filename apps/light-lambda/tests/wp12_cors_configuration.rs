use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serverless_core::*;
use std::sync::Arc;

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
#[tokio::test]
async fn cors_offline_and_startup_use_same_strict_parser_without_network() {
    for (yaml, valid) in [
        (
            "enabled: true\nallowedOrigins: [https://test.invalid]\nallowedMethods: [GET]\n",
            true,
        ),
        ("enabled: false\nallowedOrigins: []\n", true),
        ("enabled: false\nallowedMethods: [12]\n", false),
        ("enabled: true\nunknown: secret-must-not-leak\n", false),
        ("enabled: true\npathPrefixAllowed: []\n", false),
        ("enabled: [true]\n", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("values.yml"),
            "light-lambda.stage: test-only\n",
        )
        .unwrap();
        for (name, content) in [
            ("cors.yml", yaml),
            (
                "security.yml",
                "enabled: false\nenableVerifyJwt: false\nenableVerifyScope: false\n",
            ),
            (
                "unified-security.yml",
                "enabled: false\npathPrefixAuths: []\n",
            ),
            (
                "lambda-proxy.yml",
                "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {'/x@get': TestFn}\n",
            ),
            (
                "handler.yml",
                "enabled: true\nhandlers: ['com.networknt.aws.lambda.handler.middleware.cors.RequestCorsMiddleware@requestCors','com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy','com.networknt.aws.lambda.handler.middleware.cors.ResponseCorsMiddleware@responseCors']\ndefaultHandlers: [requestCors,proxy,responseCors]\n",
            ),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let offline = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let report = validate_configuration(&offline).await;
        let mut startup = light_lambda::input(Some(dir.path().into()), false).unwrap();
        startup.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
        let snapshot = initialize(startup).await;
        assert_eq!(report.valid, valid, "{:?}", report.errors);
        assert_eq!(snapshot.is_ok(), valid);
        if valid {
            assert_eq!(
                report.config_digest.as_deref(),
                Some(snapshot.unwrap().config_digest())
            );
        } else {
            assert_eq!(
                report.errors,
                vec![snapshot.err().unwrap().message().to_string()]
            );
            assert!(
                !serde_json::to_string(&report)
                    .unwrap()
                    .contains("secret-must-not-leak")
            );
        }
    }
}
