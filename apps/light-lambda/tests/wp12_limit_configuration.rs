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
        (
            "enabled: true\nkey: address\naddressKeyResolver: com.networknt.limit.key.AkamaiAddressKeyResolver\nrateLimit: 1/m\n",
            false,
        ),
        (
            "enabled: true\nkey: address\naddressKeyResolver: com.networknt.limit.key.AkamaiAddressKeyResolver\nrateLimit: 1/m\ntrustedProxyCidrs: ['203.0.113.0/24']\n",
            true,
        ),
        (
            "enabled: true\nkey: address\nrateLimit: 1/m\ntrustedProxyCidrs: ['invalid']\n",
            false,
        ),
        (
            "enabled: true\nkey: address\nrateLimit: 1/m\ntrustedProxyCidrs: ['0.0.0.0/0']\n",
            false,
        ),
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

struct CountDispatch(std::sync::atomic::AtomicUsize);
#[async_trait]
impl Backend for CountDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some("ok".into()),
            body_is_binary: false,
        })
    }
}
#[tokio::test]
async fn real_alb_service_preserves_client_quotas_behind_one_trusted_cdn_edge() {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in [
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
            "limit.yml",
            "enabled: true\nkey: address\naddressKeyResolver: com.networknt.limit.key.AkamaiAddressKeyResolver\ntrustedProxyCidrs: ['203.0.113.0/24']\nrateLimit: 1/m\naddress: {'198.51.100.1': '1/m', '198.51.100.2': '1/m'}\n",
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
    let backend = Arc::new(CountDispatch(std::sync::atomic::AtomicUsize::new(0)));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(backend.clone());
    let snapshot = initialize(input).await.unwrap();
    for (client, expected) in [
        (Some("198.51.100.1"), 200),
        (Some("198.51.100.2"), 200),
        (Some("198.51.100.1"), 429),
        (None, 500),
    ] {
        let mut headers = serde_json::json!({"x-forwarded-for":["198.51.100.1, 203.0.113.9"]});
        if let Some(client) = client {
            headers["true-client-ip"] = serde_json::json!([client]);
        }
        let event = serde_json::json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"GET","path":"/x","multiValueHeaders":headers});
        let mut context = lambda_runtime::Context::default();
        context.request_id = "test-only-proxy-trust".into();
        context.deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 5000;
        let response = light_lambda_runtime::service::handle(
            snapshot.clone(),
            lambda_runtime::LambdaEvent::new(event, context),
        )
        .await
        .unwrap();
        assert_eq!(response["statusCode"], expected);
    }
    assert_eq!(backend.0.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn proxy_allowlist_is_wired_through_embedded_limit_template_values() {
    for (cidrs, valid) in [("[]", false), ("['203.0.113.0/24']", true)] {
        let dir = tempfile::tempdir().unwrap();
        // No local limit.yml: exercise the shipped template and values binding.
        std::fs::write(dir.path().join("values.yml"), format!("light-lambda.stage: test-only\nlimit.enabled: true\nlimit.key: address\nlimit.rateLimit: 1/m\nlimit.addressKeyResolver: com.networknt.limit.key.AkamaiAddressKeyResolver\nlimit.trustedProxyCidrs: {cidrs}\n")).unwrap();
        for (name, content) in [
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
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
        assert_eq!(validate_configuration(&input).await.valid, valid);
        assert_eq!(initialize(input).await.is_ok(), valid);
    }
}
