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
async fn limit_offline_startup_key_ownership_warnings_and_identity_order_are_consistent() {
    for (yaml, valid) in [
        ("enabled: true\nrateLimit: 1/m\n", true),
        ("enabled: false\n", true),
        ("enabled: false\nheadersAlwaysSet: true\n", false),
        ("enabled: false\nconcurrentRequest: 1\n", false),
        ("enabled: false\nqueueSize: 10\n", false),
        ("enabled: true\nkey: client\nrateLimit: 1/m\n", false),
        (
            "enabled: false\nclientIdKeyResolver: secret-must-not-leak.Plugin\n",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("values.yml", "light-lambda.stage: test-only\n"),
            ("limit.yml", yaml),
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
                "enabled: true\nhandlers: ['com.networknt.aws.lambda.handler.middleware.limit.LimitMiddleware@limit','com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy']\ndefaultHandlers: [limit,proxy]\n",
            ),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let report = validate_configuration(&input).await;
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
        let result = initialize(input).await;
        assert_eq!(report.valid, valid, "{:?}", report.errors);
        assert_eq!(result.is_ok(), valid);
        if valid {
            assert_eq!(
                report.config_digest.as_deref(),
                Some(result.unwrap().config_digest())
            );
            for key in ["concurrentRequest", "queueSize", "headersAlwaysSet"] {
                assert_eq!(
                    report
                        .warnings
                        .iter()
                        .filter(|s| s.starts_with(&format!("limit.{key}:")))
                        .count(),
                    1
                );
            }
        } else {
            assert_eq!(
                report.errors,
                vec![result.err().unwrap().message().to_string()]
            );
        }
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("secret-must-not-leak")
        );
    }
}
