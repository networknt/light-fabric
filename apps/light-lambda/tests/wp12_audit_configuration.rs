use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serverless_core::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct NoDispatch(Arc<AtomicUsize>);

#[tokio::test]
async fn embedded_audit_and_mask_templates_apply_approved_values_keys_and_defaults() {
    use light_lambda_runtime::init::prepare_local;
    let dir = tempfile::tempdir().unwrap();
    let values = serde_json::json!({"light-lambda.stage":"test-only","lambda-proxy.region":"us-east-1","security.enabled":false,"security.enableVerifyJwt":false,"security.enableVerifyScope":false,"unified-security.enabled":false,"audit.enabled":true,"audit.mask":false,"audit.statusCode":false,"audit.responseTime":false,"audit.auditOnError":false,"audit.auditStackTrace":true,"audit.logLevelIsError":true,"audit.timestampFormat":"'safe-literal'","audit.headers":["x-safe-observation"],"audit.audit":["serviceId"],"audit.requestBodyMaxSize":1000,"audit.responseBodyMaxSize":2000,"mask.regex":{"requestHeader":{"x-safe-observation":"id=(.+)"}}});
    std::fs::write(dir.path().join("values.yml"), values.to_string()).unwrap();
    let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    let config = prepare_local(&input).await.unwrap();
    let audit: serde_json::Value = config
        .module_registry
        .load_config(&config, "audit.yml")
        .unwrap();
    for key in [
        "enabled",
        "mask",
        "statusCode",
        "responseTime",
        "auditOnError",
        "auditStackTrace",
        "logLevelIsError",
        "timestampFormat",
        "headers",
        "audit",
        "requestBodyMaxSize",
        "responseBodyMaxSize",
    ] {
        assert_eq!(audit[key], values[format!("audit.{key}")], "{key}");
    }
    let mask: serde_json::Value = config
        .module_registry
        .load_config(&config, "mask.yml")
        .unwrap();
    assert_eq!(mask["regex"], values["mask.regex"]);
    let report = validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    let count = Arc::new(AtomicUsize::new(0));
    let mut startup = light_lambda::input(Some(dir.path().into()), false).unwrap();
    startup.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let started = initialize(startup).await;
    assert!(
        started.is_ok(),
        "startup: {:?}",
        started.as_ref().err().map(|e| e.message())
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // The defaults remain Java-compatible apart from approved explicit opt-in.
    let defaults = tempfile::tempdir().unwrap();
    let input = light_lambda::input(Some(defaults.path().into()), false).unwrap();
    let config = prepare_local(&input).await.unwrap();
    let audit: serde_json::Value = config
        .module_registry
        .load_config(&config, "audit.yml")
        .unwrap();
    assert_eq!(audit["enabled"], false);
    assert_eq!(
        audit["headers"],
        serde_json::json!(["X-Correlation-Id", "X-Traceability-Id", "caller_id"])
    );
    assert_eq!(
        audit["audit"],
        serde_json::json!([
            "client_id",
            "user_id",
            "scope_client_id",
            "endpoint",
            "serviceId",
            "toolName"
        ])
    );
    assert_eq!(audit["requestBodyMaxSize"], 4096);
    assert_eq!(audit["responseBodyMaxSize"], 4096);
    assert!(audit["timestampFormat"].is_null());
    let mask: serde_json::Value = config
        .module_registry
        .load_config(&config, "mask.yml")
        .unwrap();
    assert_eq!(
        mask["string"],
        serde_json::json!({"uri":{"password=[^&]*":"password=******"}})
    );
    assert!(mask["regex"].is_null());
    assert!(mask["json"].is_null());
}

#[tokio::test]
async fn original_mask_test_deferred_selectors_reject_independent_of_activation_or_selection() {
    for enabled in [false, true] {
        for selected in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            configs(
                dir.path(),
                &format!(
                    "enabled: {enabled}\naudit: [requestBody, queryParameters, pathParameters]\nrequestBodyMaxSize: 1000\n"
                ),
                "{}\n",
            );
            if selected {
                let class = |id| {
                    serverless_core::compat::COMPATIBILITY
                        .iter()
                        .find(|c| c.id == id)
                        .unwrap()
                        .class
                };
                std::fs::write(dir.path().join("handler.yml"),serde_json::json!({"handlers":[format!("{}@audit",class(HandlerId::Audit)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["audit","proxy"]}).to_string()).unwrap();
            }
            let offline = validate_configuration(
                &light_lambda::input(Some(dir.path().into()), false).unwrap(),
            )
            .await;
            let count = Arc::new(AtomicUsize::new(0));
            let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
            input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
            let error = initialize(input)
                .await
                .err()
                .expect("deferred selectors must reject before publication");
            assert_eq!(offline.errors, ["audit.yml: unsafe or unbounded selector"]);
            assert_eq!(error.message(), offline.errors[0]);
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn real_audit_provider_offline_startup_settings_and_service_id_agree_without_dispatch() {
    let cases = [
        (
            "enabled: true\nheaders: []\naudit: [serviceId]\n",
            "{}\n",
            "trusted-audit-service",
            true,
        ),
        (
            "enabled: false\nheaders: []\naudit: [serviceId]\n",
            "{}\n",
            "trusted-audit-service",
            true,
        ),
        (
            "enabled: true\nheaders: []\naudit: [serviceId]\n",
            "{}\n",
            "",
            false,
        ),
        (
            "enabled: true\nheaders: []\naudit: [serviceId]\n",
            "{}\n",
            "trusted\nprivate-marker",
            false,
        ),
        (
            "enabled: false\nmask: []\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\nresponseTime: []\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\nlogLevelIsError: []\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\nrequestBodyMaxSize: 0\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\nrequestBodyMaxSize: 65536\n",
            "{}\n",
            "trusted-audit-service",
            true,
        ),
        (
            "enabled: false\nrequestBodyMaxSize: 65537\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\naudit_on_error: true\n",
            "{}\n",
            "trusted-audit-service",
            false,
        ),
        (
            "enabled: false\nheaders: [x-test-mask]\naudit: [client_id]\n",
            "regex: {requestHeader: {x-test-mask: 'id=(.+)'}, audit: {client_id: '(.*)'}}\n",
            "trusted-audit-service",
            true,
        ),
        (
            "enabled: false\nheaders: [x-test-mask]\naudit: [client_id]\n",
            "regex: '{\"requestHeader\":{\"x-test-mask\":\"id=(.+)\"}}'\n",
            "trusted-audit-service",
            true,
        ),
        (
            "enabled: false\n",
            "regex: {requestHeader: {x-test-mask: '(?=private-marker)'}}\n",
            "trusted-audit-service",
            false,
        ),
    ];
    for selected in [true, false] {
        for (audit, mask, service, valid) in cases {
            let dir = tempfile::tempdir().unwrap();
            configs(dir.path(), audit, mask);
            std::fs::write(
                dir.path().join("lambda-app.yml"),
                serde_json::json!({"lambdaAppId":service}).to_string(),
            )
            .unwrap();
            if selected {
                let class = |id| {
                    serverless_core::compat::COMPATIBILITY
                        .iter()
                        .find(|c| c.id == id)
                        .unwrap()
                        .class
                };
                std::fs::write(dir.path().join("handler.yml"),serde_json::json!({"handlers":[format!("{}@audit",class(HandlerId::Audit)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["audit","proxy"]}).to_string()).unwrap();
            }
            let offline = validate_configuration(
                &light_lambda::input(Some(dir.path().into()), false).unwrap(),
            )
            .await;
            let count = Arc::new(AtomicUsize::new(0));
            let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
            input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
            let startup = initialize(input).await;
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert_eq!(
                offline.valid, valid,
                "selected={selected} valid={valid}: {:?}",
                offline.errors
            );
            assert_eq!(startup.is_ok(), valid);
            if let Err(error) = startup {
                assert_eq!(offline.errors, vec![error.message()]);
            }
            assert!(
                !serde_json::to_string(&offline)
                    .unwrap()
                    .contains("private-marker")
            );
        }
    }
}

#[tokio::test]
async fn audit_actual_trusted_service_id_utf8_bounds_and_preparation_network_isolation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for (service, valid) in [
        ("x".repeat(1024), true),
        ("x".repeat(1025), false),
        ("é".repeat(512), true),
        ("é".repeat(512) + "x", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        configs(
            dir.path(),
            "enabled: true\nheaders: []\naudit: [serviceId]\n",
            "{}\n",
        );
        std::fs::write(
            dir.path().join("lambda-app.yml"),
            serde_json::json!({"lambdaAppId":service}).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("values.yml"),serde_json::json!({"light-lambda.stage":"test-only","client.tokenKeyServerUrl":format!("http://{}",listener.local_addr().unwrap()),"client.tokenKeyUri":"/unused"}).to_string()).unwrap();
        let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let offline = validate_configuration(&input).await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(offline.valid, valid, "{:?}", offline.errors);
        assert_eq!(startup.is_ok(), valid);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if !valid {
            assert_eq!(offline.errors, ["audit.yml: invalid trusted service id"]);
        }
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn safe_selectors_preserve_the_approved_two_mask_key_dispositions() {
    for (mask, valid) in [
        ("{}\n", true),
        ("string: null\njson: ''\n", true),
        (
            "string: {uri: {'password=[^&]*': 'password=******'}}\nregex: {requestHeader: {x-test-header: 'id=(.+)'}}\n",
            true,
        ),
        ("string: '{}'\n", false),
        ("json: {}\n", false),
        (
            "string: {requestBody: {secret: secret-must-not-leak}}\n",
            false,
        ),
        (
            "json: {responseBody: {secret: secret-must-not-leak}}\n",
            false,
        ),
        ("string: false\n", false),
        ("json: false\n", false),
        ("regex: {requestHeader: {x-test-header: false}}\n", false),
        ("json: '['\n", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        configs(
            dir.path(),
            "enabled: false\naudit: [serviceId]\nrequestBodyMaxSize: 1000\n",
            mask,
        );
        let offline =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(offline.valid, valid, "{:?}", offline.errors);
        assert_eq!(startup.is_ok(), valid);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(
            !serde_json::to_string(&offline)
                .unwrap()
                .contains("secret-must-not-leak")
        );
        if valid {
            assert_eq!(
                offline
                    .warnings
                    .iter()
                    .filter(|w| w.starts_with("mask."))
                    .count(),
                2
            );
        }
    }
}

#[tokio::test]
async fn wp12_audit_activation_real_factory_must_reach_common_preparation() {
    let dir = tempfile::tempdir().unwrap();
    configs(
        dir.path(),
        "enabled: true\nauditOnError: false\nheaders: []\naudit: [serviceId]\n",
        "{}\n",
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"), serde_json::json!({"handlers":[format!("{}@audit",class(HandlerId::Audit)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["audit","proxy"]}).to_string()).unwrap();
    let report =
        validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let startup = initialize(input).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        report.valid,
        "actual Audit wiring pending: {:?}; startup: {:?}",
        report.errors,
        startup.as_ref().err().map(|e| e.message())
    );
    assert!(startup.is_ok());
}

#[tokio::test]
async fn wp12_audit_activation_full_constructor_validation_is_required_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    configs(dir.path(), "enabled: false\nstatusCode: []\n", "{}\n");
    let report =
        validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let startup = initialize(input).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        !report.valid,
        "disabled Audit must retain strict real-constructor validation"
    );
    assert!(startup.is_err());
}

#[tokio::test]
async fn configured_apikey_header_must_not_be_disclosed_by_actual_audit() {
    for enabled in [false, true] {
        for unified in [false, true] {
            for selectors in ["[X-Test-Header]", "x-test-header", "'[\"x-test-header\"]'"] {
                let dir = tempfile::tempdir().unwrap();
                configs(
                    dir.path(),
                    &format!("enabled: {enabled}\nmask: false\nheaders: {selectors}\naudit: []\n"),
                    "{}\n",
                );
                std::fs::write(dir.path().join("apikey.yml"),
                    "enabled: false\nhashEnabled: false\npathPrefixAuths: [{pathPrefix: /, headerName: x-test-header, apiKey: test-only-audit-credential}]\n").unwrap();
                if unified {
                    std::fs::write(
                        dir.path().join("unified-security.yml"),
                        "enabled: true\npathPrefixAuths: [{prefix: /, apikey: true}]\n",
                    )
                    .unwrap();
                }
                let offline = validate_configuration(
                    &light_lambda::input(Some(dir.path().into()), false).unwrap(),
                )
                .await;
                let count = Arc::new(AtomicUsize::new(0));
                let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
                input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
                let error = initialize(input).await.err().unwrap();
                assert_eq!(
                    offline.errors,
                    vec!["audit.yml: credential header selection unsupported"]
                );
                assert_eq!(error.message(), offline.errors[0]);
                assert_eq!(count.load(Ordering::SeqCst), 0);
                let diagnostics = serde_json::to_string(&offline).unwrap();
                for secret in [
                    "x-test-header",
                    "X-Test-Header",
                    "test-only-audit-credential",
                ] {
                    assert!(!diagnostics.contains(secret));
                    assert!(!error.message().contains(secret));
                }
            }
        }
    }
}

#[tokio::test]
async fn credential_overlap_validation_preserves_safe_selectors_and_effective_defaults() {
    for (headers, credential, accepted) in [
        ("headers: [x-safe-observation]\n", "x-test-header", true),
        ("headers: []\n", "x-test-header", true),
        ("headers: null\n", "x-test-header", true),
        ("", "x-correlation-id", false),
        ("", "x-traceability-id", false),
        ("", "CALLER_ID", false),
        (
            "headers: [x-safe-observation]\n",
            "X-SAFE-OBSERVATION",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        configs(dir.path(), &format!("enabled: false\n{headers}"), "{}\n");
        std::fs::write(dir.path().join("apikey.yml"),format!(
            "enabled: false\npathPrefixAuths: '[{{\"pathPrefix\":\"/\",\"headerName\":\"{credential}\",\"apiKey\":\"test-only-credential\"}}]'\n"
        )).unwrap();
        let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let offline = validate_configuration(&input).await;
        assert_eq!(offline.valid, accepted, "{headers}");
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        assert_eq!(initialize(input).await.is_ok(), accepted);
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn malformed_credential_overlap_inputs_fail_closed_without_disclosure() {
    for (headers, rules) in [
        ("{test-only-private-marker: false}", "[]"),
        ("[123]", "[]"),
        ("[x-safe]", "'{test-only-private-marker: false}'"),
        (
            "[x-safe]",
            "[{headerName: 123, pathPrefix: /, apiKey: test-only-private-marker}]",
        ),
        ("[x-safe]", "'[malformed-test-only-private-marker'"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        configs(
            dir.path(),
            &format!("enabled: false\nheaders: {headers}\n"),
            "{}\n",
        );
        std::fs::write(
            dir.path().join("apikey.yml"),
            format!("enabled: false\npathPrefixAuths: {rules}\n"),
        )
        .unwrap();
        let offline =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let error = initialize(input).await.err().unwrap();
        assert!(!offline.valid);
        assert_eq!(offline.errors, vec![error.message().to_owned()]);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(
            !serde_json::to_string(&offline)
                .unwrap()
                .contains("test-only-private-marker")
        );
        assert!(!error.message().contains("test-only-private-marker"));
    }
}
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("Init must never dispatch")
    }
}
fn configs(dir: &std::path::Path, audit: &str, mask: &str) {
    for (file, value) in [
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
        ("audit.yml", audit),
        ("mask.yml", mask),
    ] {
        std::fs::write(dir.join(file), value).unwrap();
    }
}
#[tokio::test]
async fn response_body_selector_rejects_offline_and_startup_without_dispatch_or_diagnostic_disclosure()
 {
    for enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        configs(
            dir.path(),
            &format!(
                "enabled: {enabled}\naudit: [responseBody]\ntimestampFormat: 'synthetic-response-body-must-not-leak'\nrequestBodyMaxSize: 1000\n"
            ),
            "{}\n",
        );
        let offline =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await.err().unwrap();
        assert!(!offline.valid);
        assert_eq!(
            offline.errors,
            vec![serverless_core::unsupported::audit::RESPONSE_BODY_ERROR.to_owned()]
        );
        assert_eq!(startup.message(), offline.errors[0]);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(
            !serde_json::to_string(&offline)
                .unwrap()
                .contains("synthetic-response-body-must-not-leak")
        );
        assert!(
            !startup
                .message()
                .contains("synthetic-response-body-must-not-leak")
        );
    }
}
#[tokio::test]
async fn two_mask_keys_have_identical_strict_redacted_offline_startup_dispositions_while_disabled()
{
    for (mask, valid) in [
        ("{}\n", true),
        ("string: null\njson: ''\n", true),
        (
            "string: {uri: {'password=[^&]*': 'password=******'}}\nregex: {requestHeader: {x-test-header: 'id=(.+)'}}\n",
            true,
        ),
        ("string: '{}'\n", false),
        ("json: {}\n", false),
        (
            "string: {requestBody: {secret: secret-must-not-leak}}\n",
            false,
        ),
        (
            "json: {responseBody: {secret: secret-must-not-leak}}\n",
            false,
        ),
        ("string: false\n", false),
        ("json: false\n", false),
        ("regex: {requestHeader: {x-test-header: false}}\n", false),
        ("json: '['\n", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        configs(
            dir.path(),
            "enabled: false\naudit: [serviceId]\nrequestBodyMaxSize: 1000\n",
            mask,
        );
        let offline =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(offline.valid, valid);
        assert_eq!(startup.is_ok(), valid);
        if valid {
            assert_eq!(
                offline
                    .warnings
                    .iter()
                    .filter(|w| w.starts_with("mask."))
                    .count(),
                2
            );
        } else {
            assert_eq!(
                offline.errors,
                vec![startup.err().unwrap().message().to_owned()]
            );
        }
        assert!(
            !serde_json::to_string(&offline)
                .unwrap()
                .contains("secret-must-not-leak")
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn malformed_audit_timestamp_rejects_offline_and_startup_even_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    configs(
        dir.path(),
        "enabled: false\nheaders: []\naudit: []\ntimestampFormat: \"'synthetic-unclosed\"\n",
        "{}\n",
    );
    let offline =
        validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap()).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let startup = initialize(input).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        offline.valid,
        startup.is_ok(),
        "offline/startup dispositions agree"
    );
    assert!(
        !serde_json::to_string(&offline)
            .unwrap()
            .contains("synthetic-unclosed")
    );
    assert!(
        !offline.valid,
        "malformed supported timestamps must fail offline Init validation"
    );
    assert!(
        startup.is_err(),
        "startup must share strict timestamp validation"
    );
    assert_eq!(
        offline.errors,
        vec![startup.err().unwrap().message().to_owned()]
    );
}

struct DiscardAudit;
impl gateway_policy::observation::RecordSink for DiscardAudit {
    fn try_emit(&self, _: gateway_policy::observation::RecordKind, _: serde_json::Value) {
        panic!("configuration validation must not emit audit records")
    }
}

#[tokio::test]
async fn timestamp_constructor_offline_startup_agree_on_defaults_patterns_and_bounds() {
    use serde_json::{Value, json};
    let cases = vec![
        (None, true),
        (Some(Value::Null), true),
        (Some(json!("")), true),
        (Some(json!("'safe-literal'")), true),
        (Some(json!("yyyy-MM-dd HH:mm:ss.SSS")), true),
        (Some(json!(format!("'{}'", "é".repeat(512)))), true),
        (Some(json!(format!("'{}x'", "é".repeat(512)))), false),
        (
            Some(json!(format!("{}'safe'{}", "[".repeat(16), "]".repeat(16)))),
            true,
        ),
        (
            Some(json!(format!("{}'safe'{}", "[".repeat(17), "]".repeat(17)))),
            false,
        ),
        (Some(json!("'synthetic-unclosed")), false),
        (Some(json!("yyyy-qqqqqq")), false),
        (Some(json!(true)), false),
        (Some(json!(42)), false),
        (Some(json!(["secret-must-not-leak"])), false),
        (Some(json!({"secret":"secret-must-not-leak"})), false),
    ];
    for (timestamp, valid) in cases {
        let mut audit = json!({"enabled":false,"headers":[],"audit":[]});
        if let Some(timestamp) = timestamp {
            audit["timestampFormat"] = timestamp;
        }
        let constructor = serverless_core::handlers::audit::AuditHandler::from_value(
            &audit,
            "test-only-service".into(),
            Arc::new(DiscardAudit),
        );
        let dir = tempfile::tempdir().unwrap();
        configs(dir.path(), &serde_json::to_string(&audit).unwrap(), "{}\n");
        let offline =
            validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap())
                .await;
        let count = Arc::new(AtomicUsize::new(0));
        let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
        let startup = initialize(input).await;
        assert_eq!(constructor.is_ok(), valid);
        assert_eq!(offline.valid, valid);
        assert_eq!(startup.is_ok(), valid);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if !valid {
            let error = startup.err().unwrap();
            assert_eq!(offline.errors, vec![error.message().to_owned()]);
            assert!(!error.message().contains("secret-must-not-leak"));
            assert!(!error.message().contains("synthetic-unclosed"));
        }
    }
}
