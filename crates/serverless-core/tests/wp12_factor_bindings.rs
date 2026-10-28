//! Real neutral handlers and constructor/factory retention; no provider calls.
use http::{HeaderMap, Method};
use serde_json::json;
use serverless_core::{
    compat::COMPATIBILITY,
    handlers::{
        factors::{ApiKeyHandler, BasicHandler},
        jwt::JwtHandler,
        security::{UnifiedHandler, UnifiedSettings},
    },
    plan::{Binding, HandlerFactory},
    unsupported::validate_security,
    *,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub const HASH: &str = "1000:00000000000000000000000000000000:f8ce1e82fe4e7302204466cee3df74ee842fb760cb49b38806e573c15bb10c01797479db2b438948f1ebcdc7b0f4a7385ce41461fe988806a00c95eccb9593b5";
fn jwt() -> Arc<JwtHandler> {
    Arc::new(
        JwtHandler::new(
            validate_security(&json!({"enabled":false}), false).unwrap(),
            None,
            vec![],
        )
        .unwrap(),
    )
}
fn basic() -> Arc<BasicHandler> {
    Arc::new(BasicHandler::from_value(&json!({"enabled":true,"enableAD":false,"users":[{"username":"test-user","password":"test-only-password","paths":["/b"]}]})).unwrap())
}
fn api() -> Arc<ApiKeyHandler> {
    Arc::new(ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":"test-only-key"}]})).unwrap())
}
fn invocation(path: &str, auth: Option<&str>, key: Option<&str>) -> Invocation {
    let mut headers = HeaderMap::new();
    if let Some(auth) = auth {
        headers.insert("authorization", auth.parse().unwrap());
    }
    if let Some(key) = key {
        headers.insert("x-test-key", key.parse().unwrap());
    }
    Invocation {
        method: Method::GET,
        raw_path: path.into(),
        path: path.into(),
        raw_query: None,
        query: MultiMap::default(),
        headers,
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        response_deadline: tokio::time::Instant::now() + Duration::from_secs(3),
        cancel: CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: json!({}),
    }
}
fn compile(handler: &UnifiedHandler) -> Result<Binding, InitError> {
    handler.build(
        COMPATIBILITY
            .iter()
            .find(|c| c.id == HandlerId::Security)
            .unwrap(),
        "security",
    )
}
#[test]
fn existing_constructor_signature_and_jwt_only_factory_remain_compatible() {
    let construct: fn(UnifiedSettings, Arc<JwtHandler>) -> UnifiedHandler = UnifiedHandler::new;
    let handler = construct(
        UnifiedSettings::from_value(&json!({"pathPrefixAuths":[{"prefix":"/jwt","jwt":true}]}))
            .unwrap(),
        jwt(),
    );
    assert!(matches!(compile(&handler), Ok(Binding::Request(..))));
}
#[test]
fn incomplete_or_disabled_factor_bindings_are_init_errors() {
    for enabled in [false, true] {
        let settings = || {
            UnifiedSettings::from_value(&json!({"enabled":enabled,"pathPrefixAuths":[{"prefix":"/b","basic":true},{"prefix":"/a","apikey":true}]})).unwrap()
        };
        assert!(compile(&UnifiedHandler::new(settings(), jwt())).is_err());
        assert!(
            UnifiedHandler::new(settings(), jwt())
                .with_factor_bindings(Some(basic()), None)
                .is_err()
        );
        assert!(
            UnifiedHandler::new(settings(), jwt())
                .with_factor_bindings(None, Some(api()))
                .is_err()
        );
        let disabled = Arc::new(BasicHandler::from_value(&json!({"enabled":false})).unwrap());
        assert!(
            UnifiedHandler::new(settings(), jwt())
                .with_factor_bindings(Some(disabled), Some(api()))
                .is_err()
        );
        let disabled = Arc::new(ApiKeyHandler::from_value(&json!({"enabled":false})).unwrap());
        assert!(
            UnifiedHandler::new(settings(), jwt())
                .with_factor_bindings(Some(basic()), Some(disabled))
                .is_err()
        );
    }
}
#[tokio::test]
async fn compiled_binding_retains_real_factors_after_factory_and_inputs_are_dropped() {
    let basic = basic();
    let api = api();
    let weak_basic = Arc::downgrade(&basic);
    let weak_api = Arc::downgrade(&api);
    let handler=UnifiedHandler::new(UnifiedSettings::from_value(&json!({"pathPrefixAuths":[{"prefix":"/b","basic":true},{"prefix":"/a","apikey":true}]})).unwrap(),jwt()).with_factor_bindings(Some(basic),Some(api)).unwrap();
    let Binding::Request(binding, _) = compile(&handler).unwrap() else {
        panic!("real request binding required")
    };
    drop(handler);
    assert!(weak_basic.upgrade().is_some());
    assert!(weak_api.upgrade().is_some());
    for (path, auth, key) in [
        ("/b", Some("Basic test-user:test-only-password"), None),
        ("/a", None, Some("test-only-key")),
    ] {
        let mut state = ExchangeState::default();
        assert!(matches!(
            binding
                .on_request(&mut invocation(path, auth, key), &mut state)
                .await
                .unwrap(),
            HandlerOutcome::Continue
        ));
        assert!(
            state.principal.is_none(),
            "Basic/API-key must not fabricate JWT authorizer claims"
        );
    }
    drop(binding);
    assert!(weak_basic.upgrade().is_none());
    assert!(weak_api.upgrade().is_none());
}
#[tokio::test]
async fn direct_incomplete_execution_cannot_bypass_factory_validation_into_anonymous_success() {
    let handler = UnifiedHandler::new(
        UnifiedSettings::from_value(
            &json!({"anonymousPrefixes":["/b"],"pathPrefixAuths":[{"prefix":"/b","basic":true}]}),
        )
        .unwrap(),
        jwt(),
    );
    assert!(matches!(
        handler
            .on_request(
                &mut invocation("/b", None, None),
                &mut ExchangeState::default()
            )
            .await
            .unwrap(),
        HandlerOutcome::Reject(_)
    ));
}
#[tokio::test]
async fn basic_literal_lenient_base64_reserved_names_and_empty_paths_match_lambda_contracts() {
    let handler = basic();
    for auth in [
        "Basic test-user:test-only-password",
        "bAsIc dGVzdC11c2VyOnRlc3Qtb25seS1wYXNzd29yZA==",
        "Basic dGVzdC11c2VyOnRlc3Qtb25seS1wYXNzd29yZA",
        "Basic dGVzdC11c2VyOnRlc3Qtb25seS1wYXNzd29yZA==!",
    ] {
        assert!(matches!(
            handler
                .on_request(
                    &mut invocation("/b", Some(auth), None),
                    &mut ExchangeState::default()
                )
                .await
                .unwrap(),
            HandlerOutcome::Continue
        ));
    }
    for auth in [
        "Basic test-user:wrong",
        "Basic test-user:",
        "Basic anonymous:test-only-password",
        "Basic bearer:test-only-password",
        "Basic bm9jb2xvbg==",
        "Basic",
    ] {
        assert!(matches!(
            handler
                .on_request(
                    &mut invocation("/b", Some(auth), None),
                    &mut ExchangeState::default()
                )
                .await
                .unwrap(),
            HandlerOutcome::Reject(_)
        ));
    }
    let empty=BasicHandler::from_value(&json!({"enabled":true,"users":[{"username":"test-user","password":"test-only-password","paths":[]}]})).unwrap();
    let HandlerOutcome::Reject(error) = empty
        .on_request(
            &mut invocation("/b", Some("Basic test-user:test-only-password"), None),
            &mut ExchangeState::default(),
        )
        .await
        .unwrap()
    else {
        panic!("empty paths must deny")
    };
    assert_eq!(error.code, "ERR10071");
}
#[tokio::test]
async fn api_any_matching_rule_unmatched_and_real_shared_pbkdf2_verification() {
    let handler=ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":"first-key"},{"pathPrefix":"/a/private","headerName":"x-test-key","apiKey":"test-only-key"}]})).unwrap();
    for (path, key) in [
        ("/a/private", Some("test-only-key")),
        ("/a/private", Some("first-key")),
        ("/unmatched", None),
    ] {
        assert!(matches!(
            handler
                .on_request(
                    &mut invocation(path, None, key),
                    &mut ExchangeState::default()
                )
                .await
                .unwrap(),
            HandlerOutcome::Continue
        ));
    }
    assert!(matches!(
        handler
            .on_request(
                &mut invocation("/a", None, None),
                &mut ExchangeState::default()
            )
            .await
            .unwrap(),
        HandlerOutcome::Reject(_)
    ));
    let hash=ApiKeyHandler::from_value(&json!({"enabled":true,"hashEnabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":HASH}]})).unwrap();
    assert!(matches!(
        hash.on_request(
            &mut invocation("/a", None, Some("test-only-key")),
            &mut ExchangeState::default()
        )
        .await
        .unwrap(),
        HandlerOutcome::Continue
    ));
    assert!(matches!(
        hash.on_request(
            &mut invocation("/a", None, Some("wrong")),
            &mut ExchangeState::default()
        )
        .await
        .unwrap(),
        HandlerOutcome::Reject(_)
    ));
}
#[test]
fn strict_factor_configuration_rejects_unknown_types_ldap_and_unbounded_hash_work() {
    for value in [
        json!({"enabled":true,"enableAD":true}),
        json!({"enabled":false,"enableAD":true}),
        json!({"enabled":"true"}),
        json!({"unknown":false}),
        json!({"users":[{"username":"x","password":"y","unknown":false}]}),
        json!({"users":[{"username":"x","paths":[1]}]}),
    ] {
        assert!(BasicHandler::from_value(&value).is_err());
    }
    for value in [
        json!({"enabled":1}),
        json!({"unknown":false}),
        json!({"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"bad header","apiKey":"x"}]}),
        json!({"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-key","apiKey":"x","unknown":false}]}),
    ] {
        assert!(ApiKeyHandler::from_value(&value).is_err());
    }
    for hash in [
        "0:00:00",
        "1000001:00:00",
        "1000:0:00",
        "1000:00:gg",
        "1000:00:00:extra",
    ] {
        assert!(ApiKeyHandler::from_value(&json!({"hashEnabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":hash}]})).is_err());
    }
}

#[tokio::test]
async fn explicit_anonymous_and_bearer_permissions_do_not_create_jwt_identity() {
    let handler=BasicHandler::from_value(&json!({"enabled":true,"allowAnonymous":true,"allowBearerToken":true,"users":[{"username":"anonymous","paths":["/public"]},{"username":"bearer","paths":["/gate"]}]})).unwrap();
    for (path, auth) in [
        ("/public", None),
        ("/gate", Some("Bearer test-only-opaque")),
    ] {
        let mut state = ExchangeState::default();
        assert!(matches!(
            handler
                .on_request(&mut invocation(path, auth, None), &mut state)
                .await
                .unwrap(),
            HandlerOutcome::Continue
        ));
        assert!(state.principal.is_none());
    }
    let mut state = ExchangeState::default();
    let HandlerOutcome::Reject(error) = handler
        .on_request(&mut invocation("/private", None, None), &mut state)
        .await
        .unwrap()
    else {
        panic!("unconfigured anonymous path must deny")
    };
    assert_eq!(error.code, "ERR10071");
    assert_eq!(
        state.response_headers["www-authenticate"],
        "Basic realm=\"Default Realm\""
    );
    let missing =
        BasicHandler::from_value(&json!({"enabled":true,"allowBearerToken":true})).unwrap();
    let HandlerOutcome::Reject(error) = missing
        .on_request(
            &mut invocation("/gate", Some("Bearer test-only"), None),
            &mut ExchangeState::default(),
        )
        .await
        .unwrap()
    else {
        panic!("missing bearer policy must deny")
    };
    assert_eq!(error.status_code, 400);
    assert_eq!(error.code, "ERR10072");
}

#[tokio::test]
async fn explicitly_empty_plain_key_requires_a_present_exact_header() {
    for key in ["", " "] {
        let handler=ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":key}]})).unwrap();
        assert!(matches!(
            handler
                .on_request(
                    &mut invocation("/a", None, Some(key)),
                    &mut ExchangeState::default()
                )
                .await
                .unwrap(),
            HandlerOutcome::Continue
        ));
        assert!(matches!(
            handler
                .on_request(
                    &mut invocation("/a", None, None),
                    &mut ExchangeState::default()
                )
                .await
                .unwrap(),
            HandlerOutcome::Reject(_)
        ));
    }
    assert!(ApiKeyHandler::from_value(&json!({"enabled":true,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key"}]})).is_err());
}
