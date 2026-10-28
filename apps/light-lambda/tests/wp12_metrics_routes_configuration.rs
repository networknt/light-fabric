use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serverless_core::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct NoDispatch(Arc<AtomicUsize>);
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("offline/startup metrics route preparation must not dispatch")
    }
}

#[tokio::test]
async fn metrics_template_byte_admission_matches_offline_startup_and_exclusions() {
    for (template, active, selected, valid) in [
        (format!("/{{{}}}", "a".repeat(1021)), true, true, true),
        (format!("/{{{}}}", "a".repeat(1022)), true, true, false),
        (format!("/{}a", "é".repeat(511)), true, true, true),
        (format!("/{}aa", "é".repeat(511)), true, true, false),
        (format!("/{{{}}}", "a".repeat(1022)), false, true, true),
        (format!("/{{{}}}", "a".repeat(1022)), true, false, true),
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
        ] {
            std::fs::write(dir.path().join(file), text).unwrap();
        }
        std::fs::write(
            dir.path().join("metrics.yml"),
            format!("enabled: {active}\n"),
        )
        .unwrap();
        let chain = if selected {
            "[metrics, proxy]"
        } else {
            "[proxy]"
        };
        std::fs::write(dir.path().join("handler.yml"), format!("handlers: ['com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy', 'com.networknt.aws.lambda.handler.middleware.metrics.APMMetricsMiddleware@metrics']\ndefaultHandlers: {chain}\n")).unwrap();
        // YAML implicit keys have their own character limit. Explicit keys
        // preserve the full template and exercise the compiler's byte contract.
        std::fs::write(dir.path().join("lambda-proxy.yml"), format!("enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions:\n  ? '{template}@get'\n  : FnTest\n")).unwrap();
        let make_input = || {
            let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
            input
                .handlers
                .iter_mut()
                .find(|h| h.id == HandlerId::Metrics)
                .unwrap()
                .factory = Some(Arc::new(
                light_lambda_runtime::metrics_policy::MetricsPolicyProvider {
                    function_name: "test-only-gateway".into(),
                },
            ));
            input
        };
        let report = validate_configuration(&make_input()).await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = make_input();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(
            report.valid,
            valid,
            "bytes={}, active={active}, selected={selected}, errors={:?}",
            template.len(),
            report.errors
        );
        assert_eq!(startup.is_ok(), valid);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if !valid {
            assert!(matches!(
                &startup,
                Err(light_lambda_runtime::init_error::InitError::InvalidConfiguration(_))
            ));
            assert_eq!(
                report.errors,
                vec![startup.err().unwrap().message().to_owned()]
            );
            let diagnostic = serde_json::to_string(&report).unwrap();
            assert!(!diagnostic.contains(&template));
            assert!(
                diagnostic.contains("handler.yml: invalid execution plan or unavailable handler")
            );
        }
    }
}
#[tokio::test]
async fn metrics_route_catalogue_preparation_is_offline_and_startup_consistent() {
    for (functions, valid) in [
        (
            "{'/configured/fixed@get': FnTest, '/configured/{id}@get': FnTest, '/configured/*@get': FnTest}",
            true,
        ),
        (
            "{'/configured/synthetic-sensitive?invalid@get': FnTest}",
            false,
        ),
        ("{'/configured@get_bad': FnTest}", false),
        ("'synthetic-sensitive-malformed'", false),
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
            ("metrics.yml", "enabled: true\n"),
            (
                "handler.yml",
                "handlers: ['com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy', 'com.networknt.aws.lambda.handler.middleware.metrics.APMMetricsMiddleware@metrics']\ndefaultHandlers: [metrics, proxy]\n",
            ),
        ] {
            std::fs::write(dir.path().join(file), text).unwrap();
        }
        std::fs::write(dir.path().join("lambda-proxy.yml"),format!("enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {functions}\n")).unwrap();
        let make_input = || {
            let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
            input
                .handlers
                .iter_mut()
                .find(|h| h.id == HandlerId::Metrics)
                .unwrap()
                .factory = Some(Arc::new(
                light_lambda_runtime::metrics_policy::MetricsPolicyProvider {
                    function_name: "test-only-gateway".into(),
                },
            ));
            input
        };
        let report = validate_configuration(&make_input()).await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = make_input();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(report.valid, valid);
        assert_eq!(startup.is_ok(), valid);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if !valid {
            assert_eq!(
                report.errors,
                vec![startup.err().unwrap().message().to_owned()]
            );
        }
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("synthetic-sensitive")
        );
    }
}
