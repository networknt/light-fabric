//! Actual local-source/provider qualification; historical failing gate retained.
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
        panic!("Init must not dispatch")
    }
}

fn source_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
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
            "openapi-handler.yml",
            "enabled: true\nmultipleSpec: false\nignoreInvalidPath: false\npathSpecMapping: {}\n",
        ),
        (
            "lambda-proxy.yml",
            "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {'/x@get': test-only-business}\n",
        ),
    ] {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"), serde_json::json!({"enabled":true,"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","proxy"]}).to_string()).unwrap();
    dir
}
fn document(id: &str) -> String {
    serde_json::json!({"openapi":"3.0.3","info":{"title":"test-only","version":"1"},"paths":{"/x":{"get":{"operationId":id,"responses":{"200":{"description":"ok"}}}}}}).to_string()
}
async fn agree(dir: &std::path::Path, valid: bool) -> Option<String> {
    let input = light_lambda::input(Some(dir.into()), false).unwrap();
    let report = validate_configuration(&input).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut input = light_lambda::input(Some(dir.into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let result = initialize(input).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(report.valid, valid, "{:?}", report.errors);
    assert_eq!(result.is_ok(), valid);
    match result {
        Ok(snapshot) => assert_eq!(
            report.config_digest.as_deref(),
            Some(snapshot.config_digest())
        ),
        Err(error) => {
            assert_eq!(report.errors, vec![error.message()]);
            assert!(!error.message().contains("sensitive-marker"));
        }
    }
    report.config_digest
}

#[tokio::test]
async fn first_present_filename_wins_and_invalid_selected_source_never_falls_back() {
    let names = light_lambda_runtime::specification_policy::SOURCE_NAMES;
    for (index, name) in names.iter().enumerate() {
        let dir = source_dir();
        std::fs::write(dir.path().join(name), document("selected")).unwrap();
        let selected_digest = agree(dir.path(), true).await.unwrap();
        for lower in &names[index + 1..] {
            std::fs::write(dir.path().join(lower), "sensitive-marker: [").unwrap();
        }
        assert_eq!(agree(dir.path(), true).await.unwrap(), selected_digest);
        std::fs::write(dir.path().join(name), "sensitive-marker: [").unwrap();
        agree(dir.path(), false).await;
        std::fs::write(
            dir.path().join(name),
            vec![b' '; gateway_policy::openapi_operation::MAX_SOURCE_BYTES + 1],
        )
        .unwrap();
        agree(dir.path(), false).await;
        std::fs::remove_file(dir.path().join(name)).unwrap();
        std::fs::create_dir(dir.path().join(name)).unwrap();
        agree(dir.path(), false).await;
    }
    agree(source_dir().path(), false).await;
}

#[tokio::test]
async fn json_yaml_order_changes_digest_and_exact_profile_error_is_redacted() {
    for name in ["openapi.yml", "openapi.yaml", "openapi.json", "spec.yaml"] {
        let dir = source_dir();
        let ordered = |first: &str, second: &str| {
            format!(
                r#"{{"openapi":"3.1.0","components":{{"securitySchemes":{{"{first}":{{"type":"oauth2"}},"{second}":{{"type":"oauth2"}}}}}},"paths":{{"/x":{{"get":{{}}}}}}}}"#
            )
        };
        std::fs::write(dir.path().join(name), ordered("write", "read")).unwrap();
        let first = agree(dir.path(), true).await.unwrap();
        std::fs::write(dir.path().join(name), ordered("read", "write")).unwrap();
        assert_ne!(agree(dir.path(), true).await.unwrap(), first);
        let mut source: serde_json::Value = serde_json::from_str(&document("safe")).unwrap();
        source["servers"] = serde_json::json!([{"url":"/(?=sensitive-marker)"}]);
        std::fs::write(dir.path().join(name), source.to_string()).unwrap();
        let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let report = validate_configuration(&input).await;
        assert_eq!(
            report.errors,
            ["specification: base-path normalization profile rejected"]
        );
        agree(dir.path(), false).await;
    }
}

#[tokio::test]
async fn source_byte_boundary_and_unused_fallback_syntax_preserve_init_contracts() {
    let dir = source_dir();
    let mut source = document("boundary").into_bytes();
    source.resize(gateway_policy::openapi_operation::MAX_SOURCE_BYTES, b' ');
    std::fs::write(dir.path().join("openapi.json"), source).unwrap();
    agree(dir.path(), true).await;
    let path = dir.path().join("handler.yml");
    let mut handler: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    handler["basePath"] = serde_json::json!("(?=unused-sensitive-marker)");
    std::fs::write(&path, handler.to_string()).unwrap();
    let mut source: serde_json::Value = serde_json::from_str(&document("safe")).unwrap();
    source["servers"] = serde_json::json!([{"url":"/api"}]);
    std::fs::write(dir.path().join("openapi.json"), source.to_string()).unwrap();
    agree(dir.path(), true).await;
    handler["basePath"] = serde_json::json!(false);
    std::fs::write(&path, handler.to_string()).unwrap();
    agree(dir.path(), false).await;
}

#[tokio::test]
async fn specification_preparation_never_contacts_server_urls_or_external_references() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = source_dir();
    let mut source: serde_json::Value = serde_json::from_str(&document("local-only")).unwrap();
    source["servers"] = serde_json::json!([{"url":format!("{url}/api")}]);
    std::fs::write(dir.path().join("openapi.json"), source.to_string()).unwrap();
    agree(dir.path(), true).await;
    source["paths"]["/x"] = serde_json::json!({"$ref":format!("{url}/sensitive-marker")});
    std::fs::write(dir.path().join("openapi.json"), source.to_string()).unwrap();
    agree(dir.path(), false).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dedicated_preparation_requires_validated_specification_declaration() {
    let dir = source_dir();
    std::fs::write(dir.path().join("openapi.yml"), document("safe")).unwrap();
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Specification)
        .unwrap()
        .config_file = "health.yml";
    let report = validate_configuration(&input).await;
    assert_eq!(
        report.errors,
        ["specification: incorrect configuration file"]
    );
    let count = Arc::new(AtomicUsize::new(0));
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    assert_eq!(
        initialize(input).await.err().unwrap().message(),
        report.errors[0]
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn specification_unsupported_keys_reject_even_when_disabled_and_unselected() {
    for extra in [
        "multipleSpec: true",
        "ignoreInvalidPath: true",
        "pathSpecMapping: {sensitive-marker: secret}",
        "multipleSpec: []",
        "unknown: true",
        "enabled: invalid",
    ] {
        let dir = source_dir();
        std::fs::write(
            dir.path().join("openapi-handler.yml"),
            format!("enabled: false\n{extra}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("handler.yml"),
            "handlers: []\ndefaultHandlers: []\n",
        )
        .unwrap();
        agree(dir.path(), false).await;
    }
}

#[tokio::test]
async fn trusted_directory_embedded_precedence_and_captured_factory_isolation() {
    use light_lambda_runtime::{
        init::{HandlerFactoryProvider, prepare_local},
        specification_policy::SpecificationPolicyProvider,
    };
    use serverless_core::plan::Binding;
    let external = source_dir();
    let local = tempfile::tempdir().unwrap();
    let default = tempfile::tempdir().unwrap();
    let mut input = light_lambda::input(Some(external.path().into()), false).unwrap();
    input.local_dir = local.path().into();
    input.default_dir = Some(default.path().into());
    let embedded = config_loader::EmbeddedConfigFile {
        name: "openapi.yml",
        content: "openapi: 3.0.3\npaths: {'/x': {get: {operationId: embedded}}}\n",
    };
    let mut files = input.embedded.to_vec();
    files.push(embedded);
    input.embedded = Box::leak(files.into_boxed_slice());
    let config = prepare_local(&input).await.unwrap();
    let profile = serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Specification)
        .unwrap();
    async fn operation(
        factory: Arc<dyn serverless_core::plan::HandlerFactory>,
        profile: &serverless_core::compat::Compatibility,
    ) -> String {
        let Binding::Request(handler, _) = factory.build(profile, "spec").unwrap() else {
            panic!("real Specification binding required")
        };
        let mut inv = Invocation {
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
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            response_deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(3),
            cancel: tokio_util::sync::CancellationToken::new(),
            path_params: Default::default(),
            route: None,
            original: serde_json::Value::Null,
        };
        let mut state = ExchangeState::default();
        assert!(matches!(
            handler.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Continue
        ));
        assert_eq!(inv.path, "/x");
        state.operation.unwrap().operation_id().unwrap().to_string()
    }
    let (factory, embedded_identity) = SpecificationPolicyProvider
        .prepare_specification(&config, None)
        .unwrap();
    assert_eq!(operation(factory, profile).await, "embedded");
    let mut previous = embedded_identity;
    for (dir, id) in [
        (default.path(), "default"),
        (local.path(), "local"),
        (external.path(), "external"),
    ] {
        std::fs::write(dir.join("openapi.yml"), document(id)).unwrap();
        let (factory, identity) = SpecificationPolicyProvider
            .prepare_specification(&config, None)
            .unwrap();
        assert_ne!(identity, previous);
        previous = identity;
        std::fs::write(dir.join("openapi.yml"), document("mutated-after-capture")).unwrap();
        assert_eq!(operation(factory, profile).await, id);
        std::fs::write(dir.join("openapi.yml"), document(id)).unwrap();
    }
    // Filename order dominates directories: external JSON cannot shadow a
    // present embedded/application YAML with an earlier approved filename.
    std::fs::remove_file(external.path().join("openapi.yml")).unwrap();
    std::fs::write(external.path().join("openapi.json"), document("later-json")).unwrap();
    let (factory, _) = SpecificationPolicyProvider
        .prepare_specification(&config, None)
        .unwrap();
    assert_eq!(operation(factory, profile).await, "local");
    std::fs::write(local.path().join("openapi.yml"), "sensitive-marker: [").unwrap();
    assert!(
        SpecificationPolicyProvider
            .prepare_specification(&config, None)
            .is_err()
    );
}

#[tokio::test]
async fn qualified_local_source_must_reach_common_offline_and_startup_preparation() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
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
            "openapi-handler.yml",
            "enabled: true\nmultipleSpec: false\nignoreInvalidPath: false\npathSpecMapping: {}\n",
        ),
        (
            "lambda-proxy.yml",
            "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {'/x@get': test-only-business}\n",
        ),
        (
            "openapi.yaml",
            "openapi: 3.0.3\ninfo: {title: local-only, version: '1'}\npaths: {'/x': {get: {operationId: local-source-only, responses: {'200': {description: ok}}}}}\n",
        ),
    ] {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),serde_json::json!({"enabled":true,"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","proxy"]}).to_string()).unwrap();
    let offline = light_lambda::input(Some(dir.path().into()), false).unwrap();
    let report = validate_configuration(&offline).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut startup = light_lambda::input(Some(dir.path().into()), false).unwrap();
    startup.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let started = initialize(startup).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(
        report.valid,
        "qualified local-source wiring is pending: {:?}; startup error: {:?}",
        report.errors,
        started.as_ref().err().map(|e| e.message())
    );
    assert!(
        started.is_ok(),
        "qualified source startup must agree with offline validation"
    );
}
