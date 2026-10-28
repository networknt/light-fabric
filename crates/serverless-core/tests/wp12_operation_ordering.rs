mod security_support;
use async_trait::async_trait;
use gateway_policy::openapi_operation::SourceFormat;
use security_support::*;
use serde_json::{Value, json};
use serverless_core::{
    compat::COMPATIBILITY,
    handlers::{
        factors::{ApiKeyHandler, BasicHandler},
        security::{UnifiedHandler, UnifiedSettings},
        specification::SpecificationHandler,
    },
    plan::*,
    *,
};
use std::sync::Arc;

fn registry(jwt: Arc<serverless_core::handlers::jwt::JwtHandler>) -> Registry {
    let mut r = Registry::default();
    r.register(HandlerId::Jwt, jwt);
    let source = br#"{"openapi":"3.0.3","paths":{"/j":{"get":{"security":[{"oauth":["write"]}]}}},"components":{"securitySchemes":{"oauth":{"type":"oauth2"}}}}"#;
    r.register(
        HandlerId::Specification,
        Arc::new(SpecificationHandler::from_source(source, SourceFormat::Json).unwrap()),
    );
    r.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    r
}
fn configuration(chain: Value) -> Value {
    let class = |id| COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
    json!({"enabled":true,"handlers":[
        format!("{}@auth",class(HandlerId::Jwt)),format!("{}@spec",class(HandlerId::Specification)),
        format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":chain})
}
fn compile(r: &Registry, source: Value) -> Result<ExecutionPlan, InitError> {
    ExecutionPlan::compile(source, json!({}), r)
}
fn mixed(jwt: Arc<serverless_core::handlers::jwt::JwtHandler>) -> Arc<UnifiedHandler> {
    let settings = UnifiedSettings::from_value(&json!({"pathPrefixAuths":[
        {"prefix":"/j","jwt":true},{"prefix":"/b","basic":true},{"prefix":"/a","apikey":true}]}))
    .unwrap();
    let basic = Arc::new(
        BasicHandler::from_value(&json!({"enabled":true,"enableAD":false,
        "users":[{"username":"test-user","password":"test-only-password","paths":["/b"]}]}))
        .unwrap(),
    );
    let api = Arc::new(
        ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[
        {"pathPrefix":"/a","headerName":"x-test-key","apiKey":"test-only-key"}]}))
        .unwrap(),
    );
    assert_eq!(
        basic.operation_authorization_role(),
        OperationAuthorizationRole::None
    );
    assert_eq!(
        api.operation_authorization_role(),
        OperationAuthorizationRole::None
    );
    Arc::new(
        UnifiedHandler::new(settings, jwt)
            .with_factor_bindings(Some(basic), Some(api))
            .unwrap(),
    )
}

#[tokio::test]
async fn selected_active_repeated_and_nested_ordering_has_once_only_admission() {
    let (server, _) = server(false).await;
    let r = registry(handler(jwks::runtime(&server, config(), 2000), &["read"]));
    for chain in [
        json!(["spec", "auth", "proxy"]),
        json!(["auth", "spec", "auth", "proxy"]),
        json!(["spec", "auth", "spec", "auth", "proxy"]),
        json!(["auth", "proxy"]),
        json!(["spec", "proxy"]),
    ] {
        assert!(compile(&r, configuration(chain)).is_ok());
    }
    for chain in [
        json!(["auth", "spec", "proxy"]),
        json!(["spec", "auth", "spec", "proxy"]),
    ] {
        assert!(matches!(
            compile(&r, configuration(chain)),
            Err(InitError::Malformed(_))
        ));
    }
    let mut source = configuration(json!(["nested", "proxy"]));
    source["chains"] = json!({"nested":["inner","spec"],"inner":["auth"]});
    assert!(compile(&r, source.clone()).is_err());
    source["chains"]["nested"] = json!(["spec", "inner"]);
    assert!(compile(&r, source).is_ok());
    let mut source = configuration(json!(["auth", "proxy"]));
    source["chains"] = json!({"unreachable":["auth","spec"]});
    assert!(compile(&r, source).is_ok());
    let mut source = configuration(json!(["auth", "proxy"]));
    source["paths"] = json!([{"path":"/j","method":"get","exec":["auth","spec","proxy"]}]);
    assert!(compile(&r, source).is_err());
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn disabled_handlers_scope_settings_and_existing_errors_are_preserved() {
    let (server, _) = server(false).await;
    let mut r = registry(handler(jwks::runtime(&server, config(), 2000), &["read"]));
    let bad = configuration(json!(["auth", "spec", "proxy"]));
    r.disable(HandlerId::Specification);
    assert!(compile(&r, bad.clone()).is_ok());
    let mut r = registry(handler(jwks::runtime(&server, config(), 2000), &["read"]));
    r.disable(HandlerId::Jwt);
    assert!(
        compile(&r, bad).is_ok(),
        "metadata-only chains do not claim scope enforcement"
    );
    let mut cfg = config();
    cfg.enable_verify_scope = false;
    let r = registry(handler(jwks::runtime(&server, cfg, 2000), &["read"]));
    assert!(compile(&r, configuration(json!(["auth", "spec", "proxy"]))).is_ok());
    let r = registry(handler(jwks::runtime(&server, config(), 2000), &["read"]));
    assert_eq!(
        compile(&r, configuration(json!(["auth", "spec"]))).err(),
        Some(InitError::MissingTerminal)
    );
    assert_eq!(
        compile(&r, configuration(json!(["proxy", "auth", "spec"]))).err(),
        Some(InitError::InvalidTerminal)
    );
    let mut source = configuration(json!(["cycle", "proxy"]));
    source["chains"] = json!({"cycle":["cycle"]});
    assert!(matches!(compile(&r, source), Err(InitError::Cycle(_))));
    assert!(matches!(
        compile(&r, configuration(json!(["missing", "proxy"]))),
        Err(InitError::UnknownAlias(_))
    ));
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn mixed_factors_do_not_qualify_as_a_guaranteed_jwt_scope_check() {
    let (server, _) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let unified = mixed(jwt.clone());
    assert_eq!(
        unified.operation_authorization_role(),
        OperationAuthorizationRole::JwtScopes { guaranteed: false }
    );
    let mut r = registry(jwt);
    r.register(HandlerId::Security, unified);
    let class = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Security)
        .unwrap()
        .class;
    let mut source = configuration(json!(["spec", "unified", "proxy"]));
    source["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@unified")));
    assert!(compile(&r, source.clone()).is_err());
    source["defaultHandlers"] = json!(["spec", "unified", "auth", "proxy"]);
    assert!(compile(&r, source).is_ok());
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn jwt_only_selected_route_preserves_independent_basic_and_api_prefixes() {
    let (server, _) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let mut r = registry(jwt.clone());
    r.register(HandlerId::Security, mixed(jwt));
    let class = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Security)
        .unwrap()
        .class;
    let mut source = configuration(json!(["proxy"]));
    source["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@unified")));
    source["paths"] = json!([
        {"path":"/j","method":"get","exec":["spec","unified","proxy"]},
        {"path":"/b","method":"get","exec":["unified","proxy"]},
        {"path":"/a","method":"get","exec":["unified","proxy"]}
    ]);
    assert!(
        compile(&r, source).is_ok(),
        "selected exact /j can only use JWT; unrelated /b and /a must not disqualify it"
    );
    assert_eq!(server.requests(), 0);
}

fn configured_unified(
    jwt: Arc<serverless_core::handlers::jwt::JwtHandler>,
    prefixes: Value,
    anonymous: Value,
) -> Arc<UnifiedHandler> {
    let settings = UnifiedSettings::from_value(
        &json!({"pathPrefixAuths":prefixes,"anonymousPrefixes":anonymous}),
    )
    .unwrap();
    let basic = Arc::new(
        BasicHandler::from_value(&json!({"enabled":true,"enableAD":false,"users":[
        {"username":"test-user","password":"test-only-password","paths":["/b"]}]}))
        .unwrap(),
    );
    let api = Arc::new(
        ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[
        {"pathPrefix":"/a","headerName":"x-test-key","apiKey":"test-only-key"}]}))
        .unwrap(),
    );
    Arc::new(
        UnifiedHandler::new(settings, jwt)
            .with_factor_bindings(Some(basic), Some(api))
            .unwrap(),
    )
}
fn route_configuration(path: &str, verb: &str, chain: Value) -> Value {
    let mut source = configuration(json!(["proxy"]));
    let class = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Security)
        .unwrap()
        .class;
    source["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@unified")));
    source["paths"] = json!([{"path":path,"method":verb,"exec":chain}]);
    source
}

#[tokio::test]
async fn complete_language_prefix_proof_handles_adversarial_templates_wildcards_and_order() {
    let (server, _) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let cases = [
        (
            "/j",
            json!([{"prefix":"/b","basic":true},{"prefix":"/j","jwt":true}]),
            true,
        ),
        (
            "/j/{id}",
            json!([{"prefix":"/j/secret","basic":true},{"prefix":"/j","jwt":true}]),
            false,
        ),
        (
            "/j/*",
            json!([{"prefix":"/j/secret","apikey":true},{"prefix":"/j","jwt":true}]),
            false,
        ),
        // Declaration order, not longest-prefix sorting: later unsafe prefix is unreachable.
        (
            "/j/{id}",
            json!([{"prefix":"/j","jwt":true},{"prefix":"/j/secret","basic":true}]),
            true,
        ),
        (
            "/j/*",
            json!([{"prefix":"/j","jwt":true},{"prefix":"/j/secret","apikey":true}]),
            true,
        ),
        // Parameters cannot consume a slash or be empty; wildcard tails can.
        (
            "/j/{id}",
            json!([{"prefix":"/j//","basic":true},{"prefix":"/j/","jwt":true}]),
            true,
        ),
        (
            "/j/*",
            json!([{"prefix":"/j//","basic":true},{"prefix":"/j/","jwt":true}]),
            false,
        ),
        (
            "/j/{id}/fixed",
            json!([{"prefix":"/j/a/notfixed","basic":true},{"prefix":"/j/","jwt":true}]),
            true,
        ),
        (
            "/j/{id}/fixed",
            json!([{"prefix":"/j/a/","basic":true},{"prefix":"/j/","jwt":true}]),
            false,
        ),
        (
            "/j/*",
            json!([{"prefix":"/j/a/notfixed","basic":true},{"prefix":"/j/","jwt":true}]),
            false,
        ),
        (
            "/*",
            json!([{"prefix":"/","jwt":true},{"prefix":"/b","basic":true}]),
            true,
        ),
        (
            "/*",
            json!([{"prefix":"/b","basic":true},{"prefix":"/","jwt":true}]),
            false,
        ),
        (
            "/j/{id}",
            json!([{"prefix":"/j/a","jwt":true},{"prefix":"/j/","jwt":true},{"prefix":"/b","basic":true}]),
            true,
        ),
        (
            "/J",
            json!([{"prefix":"/j","jwt":true},{"prefix":"/J","basic":true}]),
            false,
        ),
        (
            "/j",
            json!([{"prefix":"/j","jwt":true,"basic":true}]),
            false,
        ),
        (
            "/j",
            json!([{"prefix":"/b","basic":true},{"prefix":"/elsewhere","jwt":true}]),
            false,
        ),
    ];
    for (path, prefixes, accepted) in cases {
        let mut r = registry(jwt.clone());
        r.register(
            HandlerId::Security,
            configured_unified(jwt.clone(), prefixes, json!([])),
        );
        for verb in ["GET", "post", "gEt", "HEAD", "OPTIONS"] {
            let result = compile(
                &r,
                route_configuration(path, verb, json!(["spec", "unified", "proxy"])),
            );
            assert_eq!(result.is_ok(), accepted, "complete language {path}@{verb}");
        }
    }
    for (path, anonymous, accepted) in [
        ("/j", json!(["/j/private"]), true),
        ("/j/{id}", json!(["/j/private"]), false),
        ("/j/*", json!(["/j/private"]), false),
        ("/j/{id}/fixed", json!(["/j/a/notfixed"]), true),
        ("/j/*", json!(["/j/a/notfixed"]), false),
    ] {
        let mut r = registry(jwt.clone());
        r.register(
            HandlerId::Security,
            configured_unified(
                jwt.clone(),
                json!([
            {"prefix":"/j/","jwt":true},{"prefix":"/j","jwt":true},{"prefix":"/b","basic":true}]),
                anonymous,
            ),
        );
        assert_eq!(
            compile(
                &r,
                route_configuration(path, "get", json!(["spec", "unified", "proxy"]))
            )
            .is_ok(),
            accepted
        );
    }
    assert_eq!(server.requests(), 0);
}

struct PotentialPathMutator;
#[async_trait]
impl RequestHandler for PotentialPathMutator {
    fn id(&self) -> HandlerId {
        HandlerId::RequestTransformer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        inv.path = "/b".into(); // Harness only, not a supported rule implementation.
        Ok(HandlerOutcome::Continue)
    }
}

#[tokio::test]
async fn unknown_default_mutable_and_nested_domains_never_upgrade_guards() {
    let (server, _) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let unified = mixed(jwt.clone());
    let global = unified.operation_authorization_role();
    assert_eq!(
        unified.operation_authorization_role_for_selection(&OperationSelection::default()),
        global
    );
    let default_delegate: Arc<dyn RequestHandler> = Arc::new(PotentialPathMutator);
    assert_eq!(
        default_delegate.operation_authorization_role_for_selection(&OperationSelection::default()),
        OperationAuthorizationRole::None
    );
    let mut r = registry(jwt);
    r.register(HandlerId::Security, unified);
    let mut source = route_configuration("/j", "get", json!(["spec", "unified", "proxy"]));
    source["chains"] = json!({"outer":["spec","inner"],"inner":["unified"]});
    source["paths"][0]["exec"] = json!(["outer", "proxy"]);
    assert!(compile(&r, source.clone()).is_ok());
    source["paths"][0]["exec"] = json!(["spec", "unified", "spec", "unified", "proxy"]);
    assert!(compile(&r, source.clone()).is_ok());
    source["paths"][0]["exec"] = json!(["unified", "spec", "proxy"]);
    assert!(compile(&r, source.clone()).is_err());
    source["paths"] = json!([]);
    source["defaultHandlers"] = json!(["spec", "unified", "proxy"]);
    assert!(
        compile(&r, source.clone()).is_err(),
        "unknown default domain keeps global result"
    );
    r.register(
        HandlerId::RequestTransformer,
        Arc::new(One(Binding::Request(
            Arc::new(PotentialPathMutator),
            Default::default(),
        ))),
    );
    let class = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::RequestTransformer)
        .unwrap()
        .class;
    source = route_configuration("/j", "get", json!(["spec", "mutator", "unified", "proxy"]));
    source["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@mutator")));
    assert!(
        compile(&r, source.clone()).is_err(),
        "path/method-mutating predecessor invalidates selected facts"
    );
    r.disable(HandlerId::RequestTransformer);
    assert!(
        compile(&r, source.clone()).is_ok(),
        "removed handlers cannot invalidate the domain"
    );
    source["chains"] = json!({"unselected":["mutator"]});
    assert!(compile(&r, source).is_ok());
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn selected_domains_do_not_relax_jwt_scope_mock_or_skip_guards() {
    let (server, _) = server(false).await;
    let mut cfg = config();
    cfg.enable_verify_scope = false;
    let jwt = handler(jwks::runtime(&server, cfg, 2000), &["read"]);
    let unified = mixed(jwt.clone());
    assert_eq!(
        unified.operation_authorization_role(),
        OperationAuthorizationRole::None
    );
    let mut r = registry(jwt);
    r.register(HandlerId::Security, unified);
    assert!(
        compile(
            &r,
            route_configuration("/j", "get", json!(["unified", "spec", "proxy"]))
        )
        .is_ok(),
        "explicitly disabled scope verification keeps its existing behavior"
    );
    let mut cfg = config();
    cfg.skip_path_prefixes = vec!["/unrelated-test-skip".into()];
    let runtime = jwks::runtime(&server, cfg, 2000);
    let value = serde_json::to_value(&runtime.config).unwrap();
    let settings = serverless_core::unsupported::validate_security(&value, true).unwrap();
    let jwt = Arc::new(
        serverless_core::handlers::jwt::JwtHandler::new(
            settings,
            Some(Arc::new(runtime)),
            vec!["read".into()],
        )
        .unwrap(),
    );
    let mut r = registry(jwt.clone());
    r.register(HandlerId::Security, mixed(jwt));
    assert!(
        compile(
            &r,
            route_configuration("/j", "get", json!(["spec", "unified", "proxy"]))
        )
        .is_err(),
        "context cannot strengthen the underlying JWT skip guard"
    );
    let mut value = serde_json::to_value(config()).unwrap();
    value["enableMockJwt"] = json!(true);
    let settings = serverless_core::unsupported::validate_security(&value, true).unwrap();
    let jwt = Arc::new(
        serverless_core::handlers::jwt::JwtHandler::new(settings, None, vec!["read".into()])
            .unwrap(),
    );
    let unified = mixed(jwt);
    assert_eq!(
        unified.operation_authorization_role_for_selection(&OperationSelection::default()),
        OperationAuthorizationRole::None
    );
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn refined_real_pipeline_preserves_factor_routes_scope_denials_cache_and_warm_state() {
    use std::sync::atomic::Ordering;
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let mut r = registry(jwt.clone());
    r.register(HandlerId::Security, mixed(jwt));
    let mut source = route_configuration("/j", "gEt", json!(["spec", "unified", "proxy"]));
    source["paths"].as_array_mut().unwrap().extend([
        json!({"path":"/b","method":"get","exec":["unified","proxy"]}),
        json!({"path":"/a","method":"get","exec":["unified","proxy"]}),
    ]);
    let plan = compile(&r, source).unwrap();
    assert_eq!(
        server.requests(),
        0,
        "compiler admission cannot resolve a provider"
    );
    let probe = Arc::new(Probe::default());
    let pipeline = Pipeline::new(Arc::new(plan), vec![probe.clone()]);
    let mut c = claims("cache-route-user");
    c["scope"] = json!("read");
    let read = signed(&key, c);
    let both = signed(&key, claims("cache-route-user"));
    for _ in 0..2 {
        denied(
            &pipeline,
            &probe,
            invocation(Some(&format!("Bearer {read}")), "/j"),
            403,
            "ERR10005",
        )
        .await;
        let before = probe.dispatches.load(Ordering::SeqCst);
        let response = pipeline
            .execute(
                invocation(Some(&format!("Bearer {both}")), "/j"),
                probe.as_ref(),
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(probe.dispatches.load(Ordering::SeqCst), before + 1);
        use base64::Engine;
        let basic =
            base64::engine::general_purpose::STANDARD.encode("test-user:test-only-password");
        assert_eq!(
            pipeline
                .execute(
                    invocation(Some(&format!("Basic {basic}")), "/b"),
                    probe.as_ref()
                )
                .await
                .status,
            200
        );
        let mut api = invocation(None, "/a");
        api.headers
            .insert("x-test-key", "test-only-key".parse().unwrap());
        assert_eq!(pipeline.execute(api, probe.as_ref()).await.status, 200);
        denied(
            &pipeline,
            &probe,
            invocation(Some(&format!("Basic {basic}")), "/j"),
            401,
            "ERR10000",
        )
        .await;
        let mut api = invocation(None, "/j");
        api.headers
            .insert("x-test-key", "test-only-key".parse().unwrap());
        denied(&pipeline, &probe, api, 401, "ERR10002").await;
    }
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 6);
    assert_eq!(
        probe.final_principals.lock().unwrap().len(),
        12,
        "one finalization per invocation"
    );
    assert_eq!(
        server.requests(),
        1,
        "configured cache/shared key coordination retained"
    );
    let mut unmatched_method = invocation(Some(&format!("Bearer {both}")), "/j");
    unmatched_method.method = http::Method::POST;
    let before = probe.dispatches.load(Ordering::SeqCst);
    assert_ne!(
        pipeline
            .execute(unmatched_method, probe.as_ref())
            .await
            .status,
        200
    );
    assert_eq!(
        probe.dispatches.load(Ordering::SeqCst),
        before,
        "GET selector facts cannot authorize unmatched POST dispatch"
    );
}

#[tokio::test]
async fn admitted_template_languages_match_actual_prefix_selection_on_adversarial_paths() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let token = signed(&key, claims("domain-proof-user"));
    let candidates = [
        "/",
        "/j",
        "/j/",
        "/j/a",
        "/j/secret",
        "/j/secretish",
        "/j//",
        "/j/a/fixed",
        "/j/a/notfixed",
        "/j/a/notfixed/fixed",
        "/j/secret/fixed",
        "/j/%2F",
        "/j/..",
        "/b",
        "/a",
    ];
    for (path, prefixes, anonymous) in [
        (
            "/j/{id}",
            json!([{"prefix":"/j","jwt":true},{"prefix":"/j/secret","basic":true}]),
            json!([]),
        ),
        (
            "/j/*",
            json!([{"prefix":"/j","jwt":true},{"prefix":"/j/secret","apikey":true}]),
            json!([]),
        ),
        (
            "/j/{id}/fixed",
            json!([{"prefix":"/j/a/notfixed","basic":true},{"prefix":"/j/","jwt":true}]),
            json!(["/j/a/notfixed"]),
        ),
        (
            "/j/{id}",
            json!([{"prefix":"/j//","basic":true},{"prefix":"/j/","jwt":true}]),
            json!([]),
        ),
    ] {
        let unified = configured_unified(jwt.clone(), prefixes, anonymous);
        let mut r = registry(jwt.clone());
        r.register(HandlerId::Security, unified.clone());
        let plan = compile(
            &r,
            route_configuration(path, "GET", json!(["spec", "unified", "proxy"])),
        )
        .unwrap();
        for candidate in candidates {
            let mut inv = invocation(Some(&format!("Bearer {token}")), candidate);
            if !plan
                .handler_ids(&inv)
                .unwrap()
                .contains(&HandlerId::Specification)
            {
                continue;
            }
            inv.headers
                .insert("x-test-key", "test-only-key".parse().unwrap());
            let mut state = ExchangeState::default();
            assert!(matches!(
                unified.on_request(&mut inv, &mut state).await.unwrap(),
                HandlerOutcome::Continue
            ));
            assert_eq!(
                state.principal.as_ref().unwrap().principal_id(),
                "domain-proof-user",
                "every path admitted by {path}, including {candidate}, must actually select JWT"
            );
        }
    }
    assert_eq!(server.requests(), 1);
}
