mod security_support;
use async_trait::async_trait;
use gateway_policy::openapi_operation::SourceFormat;
use security_support::*;
use serde_json::{Value, json};
use serverless_core::{
    compat::COMPATIBILITY,
    handlers::{
        security::{UnifiedHandler, UnifiedSettings},
        specification::SpecificationHandler,
    },
    plan::*,
    *,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn source(reverse: bool) -> String {
    let schemes = if reverse {
        r#""a":{"type":"oauth2"},"z":{"type":"oauth2"}"#
    } else {
        r#""z":{"type":"oauth2"},"a":{"type":"oauth2"}"#
    };
    format!(
        r#"{{"openapi":"3.0.3","paths":{{
        "/z-first":{{"get":{{"security":[{{"a":["read"],"z":["write","admin"]}}]}}}},
        "/a-only":{{"get":{{"security":[{{"a":["read"]}}]}}}},
        "/multiple":{{"get":{{"security":[{{"other":[]}},{{"a":["read"],"z":["write"]}},{{"z":["admin"]}}]}}}},
        "/empty":{{"get":{{"security":[{{"z":[]}}]}}}},
        "/no-required":{{"get":{{"security":[]}}}}
    }},"components":{{"securitySchemes":{{{schemes},"other":{{"type":"http","scheme":"basic"}}}}}}}}"#
    )
}

type Finalized = (u16, u32, bool, Option<String>);
#[derive(Default)]
struct Counts {
    responses: AtomicUsize,
    finalized: Mutex<Vec<Finalized>>,
}
#[async_trait]
impl ResponseHandler for Counts {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        _: &Invocation,
        _: &mut ExchangeState,
        _: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.responses.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        self.finalized.lock().unwrap().push((
            response.status.as_u16(),
            state.metrics.dispatches,
            state.metrics.finalized,
            state.principal.as_ref().map(|p| p.principal_id().into()),
        ));
    }
}

fn pipe(
    auth: Arc<dyn HandlerFactory>,
    id: HandlerId,
    spec: Option<SpecificationHandler>,
    probe: Arc<Probe>,
    counts: Arc<Counts>,
) -> Pipeline {
    let mut registry = Registry::default();
    registry.register(id, auth);
    registry.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    registry.register(
        HandlerId::ResponseHeader,
        Arc::new(One(Binding::Response(counts.clone()))),
    );
    let class = |id| COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
    let mut handlers = vec![
        format!("{}@auth", class(id)),
        format!("{}@proxy", class(HandlerId::Proxy)),
        format!("{}@response", class(HandlerId::ResponseHeader)),
    ];
    let mut chain = vec!["auth", "proxy", "response"];
    if let Some(spec) = spec {
        registry.register(HandlerId::Specification, Arc::new(spec));
        handlers.push(format!("{}@spec", class(HandlerId::Specification)));
        chain.insert(0, "spec");
    }
    let plan = ExecutionPlan::compile(
        json!({"enabled":true,"handlers":handlers,"defaultHandlers":chain}),
        json!({}),
        &registry,
    )
    .unwrap();
    Pipeline::new(Arc::new(plan), vec![probe, counts])
}
fn qualified(reverse: bool) -> SpecificationHandler {
    SpecificationHandler::from_source(source(reverse).as_bytes(), SourceFormat::Json).unwrap()
}

#[tokio::test]
async fn normalized_basepath_real_cached_token_scopes_are_reevaluated_per_invocation() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let source = source(false).replacen(
        "\"paths\":",
        "\"servers\":[{\"url\":\"/api\"}],\"paths\":",
        1,
    );
    let spec = SpecificationHandler::from_source(source.as_bytes(), SourceFormat::Json).unwrap();
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let pipeline = pipe(
        jwt,
        HandlerId::Jwt,
        Some(spec),
        probe.clone(),
        counts.clone(),
    );
    let mut claim = claims("normalized-cache-user");
    claim["scope"] = json!("read");
    let token = signed(&key, claim);
    for (path, status, code) in [
        ("/api/a-only", 200, None),
        ("/api/z-first", 403, Some("ERR10005")),
        ("/api/a-only", 200, None),
        ("/api/z-first", 403, Some("ERR10005")),
    ] {
        outcome(
            &pipeline,
            &probe,
            &counts,
            invocation(Some(&format!("Bearer {token}")), path),
            status,
            code,
            1,
        )
        .await;
    }
    assert_eq!(server.requests(), 1);
}
async fn outcome(
    pipe: &Pipeline,
    probe: &Probe,
    counts: &Counts,
    inv: Invocation,
    status: u16,
    code: Option<&str>,
    response_policies: usize,
) {
    let dispatches = probe.dispatches.load(Ordering::SeqCst);
    let finals = counts.finalized.lock().unwrap().len();
    let responses = counts.responses.load(Ordering::SeqCst);
    let response = pipe.execute(inv, probe).await;
    assert_eq!(response.status.as_u16(), status);
    if let Some(code) = code {
        let body: Value = serde_json::from_slice(response.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["code"], code);
    }
    let calls = u32::from(status == 200);
    assert_eq!(
        probe.dispatches.load(Ordering::SeqCst),
        dispatches + calls as usize
    );
    assert_eq!(
        counts.responses.load(Ordering::SeqCst),
        responses + response_policies
    );
    let observed = counts.finalized.lock().unwrap();
    assert_eq!(observed.len(), finals + 1);
    assert_eq!(observed.last().unwrap().0, status);
    assert_eq!(observed.last().unwrap().1, calls);
    assert!(observed.last().unwrap().2);
    assert_eq!(observed.last().unwrap().3.is_some(), status == 200);
}

#[tokio::test]
async fn real_cached_token_operation_selection_static_independence_and_default_callers() {
    let (server, key) = server(false).await;
    let cfg = config();
    assert!(cfg.enable_jwt_cache);
    let jwt = handler(jwks::runtime(&server, cfg, 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let ordered = pipe(
        jwt.clone(),
        HandlerId::Jwt,
        Some(qualified(false)),
        probe.clone(),
        counts.clone(),
    );
    let reversed = pipe(
        jwt.clone(),
        HandlerId::Jwt,
        Some(qualified(true)),
        probe.clone(),
        counts.clone(),
    );
    let default = pipe(jwt, HandlerId::Jwt, None, probe.clone(), counts.clone());
    let mut claim = claims("same-cached-user");
    claim["scope"] = json!("read");
    let token = signed(&key, claim);
    // This token satisfies only the lexically selected scheme, never z/write.
    for (pipeline, path, status, code) in [
        (&ordered, "/a-only", 200, None),
        (&ordered, "/z-first", 403, Some("ERR10005")),
        (&ordered, "/multiple", 403, Some("ERR10005")),
        (&reversed, "/z-first", 200, None),
        (&ordered, "/empty", 200, None),
        (&ordered, "/no-required", 200, None),
        (&default, "/z-first", 200, None),
        (&ordered, "/z-first", 403, Some("ERR10005")),
    ] {
        let mut inv = invocation(Some(&format!("Bearer {token}")), path);
        inv.headers
            .insert("x-operation-scopes", "read".parse().unwrap());
        inv.original = json!({"operation":{"security":[{"z":[]}]},"scope":"write"});
        outcome(pipeline, &probe, &counts, inv, status, code, 1).await;
    }
    for (scopes, status) in [
        ("write", 403),
        ("read write", 200),
        ("read admin", 200),
        ("", 403),
    ] {
        let mut claim = claims("scope-user");
        claim["scope"] = json!(scopes);
        let token = signed(&key, claim);
        outcome(
            &ordered,
            &probe,
            &counts,
            invocation(Some(&format!("Bearer {token}")), "/z-first"),
            status,
            (status != 200).then_some("ERR10005"),
            1,
        )
        .await;
    }
    assert_eq!(
        server.requests(),
        1,
        "real shared verifier cache/cooldown retained"
    );
}

#[tokio::test]
async fn cancellation_during_real_verification_discards_late_authorization() {
    let (server, key) = server(true).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let pipeline = pipe(
        jwt.clone(),
        HandlerId::Jwt,
        Some(qualified(false)),
        probe.clone(),
        counts.clone(),
    );
    let default = pipe(jwt, HandlerId::Jwt, None, probe.clone(), counts.clone());
    let mut claim = claims("cancelled-cached-user");
    claim["scope"] = json!("read");
    let token = signed(&key, claim);
    let inv = invocation(Some(&format!("Bearer {token}")), "/z-first");
    let cancel = inv.cancel.clone();
    let mut work = Box::pin(outcome(
        &pipeline,
        &probe,
        &counts,
        inv,
        504,
        Some("ERR14017"),
        1,
    ));
    tokio::select! {
        _ = server.request_started() => {},
        _ = &mut work => panic!("verification must wait for the held real provider"),
    }
    cancel.cancel();
    work.await;
    server.release(1);
    // Late shared key/cache completion must not grant per-operation authorization.
    outcome(
        &pipeline,
        &probe,
        &counts,
        invocation(Some(&format!("Bearer {token}")), "/z-first"),
        403,
        Some("ERR10005"),
        1,
    )
    .await;
    outcome(
        &default,
        &probe,
        &counts,
        invocation(Some(&format!("Bearer {token}")), "/z-first"),
        200,
        None,
        1,
    )
    .await;
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn specification_after_only_jwt_scope_check_is_rejected_at_init() {
    let (server, _) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let mut registry = Registry::default();
    registry.register(HandlerId::Jwt, jwt);
    registry.register(HandlerId::Specification, Arc::new(qualified(false)));
    registry.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    let class = |id| COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
    let compiled = ExecutionPlan::compile(
        json!({"enabled":true,"handlers":[
        format!("{}@auth",class(HandlerId::Jwt)),
        format!("{}@spec",class(HandlerId::Specification)),
        format!("{}@proxy",class(HandlerId::Proxy))
    ],"defaultHandlers":["auth","spec","proxy"]}),
        json!({}),
        &registry,
    );
    assert!(
        compiled.is_err(),
        "selected operation metadata after the only JWT scope check must not reach dispatch"
    );
    assert_eq!(
        server.requests(),
        0,
        "compiler rejection requires no provider work"
    );
}

#[tokio::test]
async fn real_secondary_precedence_and_unified_jwt_branch_use_operation_scopes() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let unified = UnifiedHandler::new(
        UnifiedSettings::from_value(&json!({"pathPrefixAuths":[{"prefix":"/","jwt":true}]}))
            .unwrap(),
        jwt.clone(),
    );
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let primary = signed(&key, claims("primary-user"));
    for (auth, id) in [
        (jwt as Arc<dyn HandlerFactory>, HandlerId::Jwt),
        (
            Arc::new(unified) as Arc<dyn HandlerFactory>,
            HandlerId::Security,
        ),
    ] {
        let pipeline = pipe(
            auth,
            id,
            Some(qualified(false)),
            probe.clone(),
            counts.clone(),
        );
        for (scope, status) in [("read", 403), ("write", 403), ("read write", 200)] {
            let mut claim = claims("secondary-user");
            claim["scope"] = json!(scope);
            let secondary = signed(&key, claim);
            let mut inv = invocation(Some(&format!("Bearer {primary}")), "/z-first");
            inv.headers.insert(
                "x-scope-token",
                format!("Bearer {secondary}").parse().unwrap(),
            );
            outcome(
                &pipeline,
                &probe,
                &counts,
                inv,
                status,
                (status != 200).then_some("ERR10006"),
                1,
            )
            .await;
        }
        outcome(
            &pipeline,
            &probe,
            &counts,
            invocation(Some(&format!("Bearer {primary}")), "/z-first"),
            200,
            None,
            1,
        )
        .await;
        let mut inv = invocation(Some(&format!("Bearer {primary}")), "/z-first");
        inv.headers
            .insert("x-scope-token", "Bearer invalid-secondary".parse().unwrap());
        outcome(&pipeline, &probe, &counts, inv, 401, Some("ERR10003"), 1).await;
    }
    assert!(
        probe
            .principals
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.as_deref() == Some("primary-user"))
    );
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn unqualified_metadata_invalid_tokens_and_warm_denials_fail_closed() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let unqualified =
        SpecificationHandler::from_document(&serde_json::from_str(&source(false)).unwrap())
            .unwrap();
    let unavailable = pipe(
        jwt.clone(),
        HandlerId::Jwt,
        Some(unqualified),
        probe.clone(),
        counts.clone(),
    );
    let pipeline = pipe(
        jwt,
        HandlerId::Jwt,
        Some(qualified(false)),
        probe.clone(),
        counts.clone(),
    );
    let token = signed(&key, claims("warm-user"));
    outcome(
        &unavailable,
        &probe,
        &counts,
        invocation(Some(&format!("Bearer {token}")), "/z-first"),
        500,
        Some("ERR14004"),
        1,
    )
    .await;
    outcome(
        &pipeline,
        &probe,
        &counts,
        invocation(Some(&format!("Bearer {token}")), "/z-first"),
        200,
        None,
        1,
    )
    .await;
    for auth in [None, Some("Bearer invalid-token")] {
        outcome(
            &pipeline,
            &probe,
            &counts,
            invocation(auth, "/z-first"),
            401,
            Some(if auth.is_none() {
                "ERR10002"
            } else {
                "ERR10000"
            }),
            1,
        )
        .await;
    }
    for (field, value, code) in [
        ("exp", json!(jwks::now() - 120), "ERR10001"),
        ("nbf", json!(jwks::now() + 120), "ERR10000"),
        ("iss", json!("invalid-issuer"), "ERR10000"),
        ("aud", json!("invalid-audience"), "ERR10000"),
    ] {
        let mut claim = claims("invalid-user");
        claim[field] = value;
        let token = signed(&key, claim);
        outcome(
            &pipeline,
            &probe,
            &counts,
            invocation(Some(&format!("Bearer {token}")), "/z-first"),
            401,
            Some(code),
            1,
        )
        .await;
    }
    outcome(
        &pipeline,
        &probe,
        &counts,
        invocation(Some(&format!("Bearer {token}")), "/z-first"),
        200,
        None,
        1,
    )
    .await;
}

#[tokio::test]
async fn operation_scope_pipeline_deadline_cancellation_and_held_verifier_zero_dispatch() {
    let (server, key) = server(true).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let counts = Arc::new(Counts::default());
    let pipeline = pipe(
        jwt,
        HandlerId::Jwt,
        Some(qualified(false)),
        probe.clone(),
        counts.clone(),
    );
    let token = signed(&key, claims("held-user"));
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/z-first");
    inv.deadline = tokio::time::Instant::now() + Duration::from_millis(40);
    outcome(&pipeline, &probe, &counts, inv, 504, Some("ERR14017"), 1).await;
    server.release(1);
    let inv = invocation(Some(&format!("Bearer {token}")), "/z-first");
    inv.cancel.cancel();
    outcome(&pipeline, &probe, &counts, inv, 504, Some("ERR14017"), 1).await;
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/z-first");
    inv.deadline = tokio::time::Instant::now();
    outcome(&pipeline, &probe, &counts, inv, 504, Some("ERR14017"), 1).await;
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 0);
}
