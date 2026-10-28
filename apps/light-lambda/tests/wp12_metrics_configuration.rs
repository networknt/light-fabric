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
async fn approved_metrics_label_disposition_is_identical_offline_and_at_startup() {
    for (settings, valid) in [
        ("", true),
        (
            "productName: http-sidecar\nsendScopeClientId: false\nsendCallerId: false\nsendIssuer: false\nissuerRegex: ''\n",
            true,
        ),
        ("productName: secret-must-not-leak\n", false),
        ("sendScopeClientId: true\n", false),
        ("sendCallerId: true\n", false),
        ("sendIssuer: true\n", false),
        ("issuerRegex: secret-must-not-leak\n", false),
        ("sendIssuer: 'false'\n", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        for (file, text) in [
            ("values.yml", "light-lambda.stage: test-only\n"),
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
                "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {'/x@get': FnTest}\n",
            ),
            (
                "handler.yml",
                "handlers: ['com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy']\ndefaultHandlers: [proxy]\n",
            ),
        ] {
            std::fs::write(dir.path().join(file), text).unwrap();
        }
        std::fs::write(
            dir.path().join("metrics.yml"),
            format!("enabled: false\n{settings}"),
        )
        .unwrap();
        let report =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
        let startup = initialize(input).await;
        assert_eq!(report.valid, valid);
        assert_eq!(startup.is_ok(), valid);
        if valid {
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .filter(|w| w.starts_with("metrics."))
                    .count(),
                14
            );
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .filter(|w| [
                        "metrics.productName:",
                        "metrics.sendScopeClientId:",
                        "metrics.sendCallerId:",
                        "metrics.sendIssuer:",
                        "metrics.issuerRegex:"
                    ]
                    .iter()
                    .any(|key| w.starts_with(key)))
                    .count(),
                5
            );
        } else {
            assert_eq!(
                report.errors,
                vec![startup.err().unwrap().message().to_owned()]
            );
        }
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("secret-must-not-leak")
        );
    }
}
