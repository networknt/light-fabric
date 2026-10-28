pub mod support;
use gateway_policy::openapi_operation::Dialect;
use serde_json::{Value, json};
use serverless_core::{handlers::specification::SpecificationHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;

fn document(version: &str) -> Value {
    json!({"openapi":version,"info":{"title":"local-source-only","version":"1"},"security":[{"jwt":["read"]}],"paths":{
        "/{specId}":{"get":{"operationId":"configured-operation","responses":{"200":{"description":"ok"}}}},
        "/literal":{"post":{"operationId":"literal-post","security":[]}},
        "/overridden":{"get":{"operationId":"override","security":[{"jwt":["write"],"key":[]},{}]}}
    }})
}

#[tokio::test]
async fn trusted_specification_server_base_path_selects_operation_without_changing_ingress() {
    for version in ["3.0.3", "3.1.0"] {
        for url in ["/api", "https://test-only.invalid/api"] {
            let source = serde_json::to_vec(&json!({
                "openapi":version,"info":{"title":"local-only","version":"1"},
                "servers":[{"url":url}],
                "paths":{"/pets/{specId}":{"get":{"operationId":"trusted-pet-operation","responses":{"200":{"description":"ok"}}}}}
            })).unwrap();
            let handler = SpecificationHandler::from_source(
                &source,
                gateway_policy::openapi_operation::SourceFormat::Json,
            )
            .unwrap();
            let mut inv = invocation();
            inv.path = "/api/pets/test-only-private-value".into();
            inv.route = Some(RouteMatch {
                template: "/api/pets/{handlerId}".into(),
                target: None,
            });
            let mut state = ExchangeState::default();
            assert!(
                matches!(
                    handler.on_request(&mut inv, &mut state).await.unwrap(),
                    HandlerOutcome::Continue
                ),
                "trusted local servers base path must participate in specification matching"
            );
            assert_eq!(inv.path, "/api/pets/test-only-private-value");
            assert_eq!(
                inv.route.as_ref().unwrap().template,
                "/api/pets/{handlerId}"
            );
            let operation = state.operation.as_ref().unwrap();
            assert_eq!(operation.path_template(), "/pets/{specId}");
            assert_eq!(operation.operation_id(), Some("trusted-pet-operation"));
        }
    }
}

#[tokio::test]
async fn java_regex_character_defaults_must_not_broaden_specification_matching() {
    for (base, path) in [(r"/\d+", "/１２/pets/1"), ("/(?i:k)", "/K/pets/1")] {
        let source = serde_json::to_vec(&json!({
            "openapi":"3.0.3","info":{"title":"local-only","version":"1"},
            "servers":[{"url":base}],
            "paths":{"/pets/{specId}":{"get":{"operationId":"must-not-gain-access","responses":{"200":{"description":"ok"}}}}}
        })).unwrap();
        let handler = SpecificationHandler::from_source(
            &source,
            gateway_policy::openapi_operation::SourceFormat::Json,
        )
        .unwrap();
        let mut inv = invocation();
        inv.path = path.into();
        let mut state = ExchangeState::default();
        let HandlerOutcome::Reject(error) = handler.on_request(&mut inv, &mut state).await.unwrap()
        else {
            panic!("Java character defaults must not authorize a different operation");
        };
        assert_eq!(error.response().status, 404);
        assert!(state.operation.is_none());
        assert!(state.principal.is_none());
        assert_eq!(inv.path, path);
    }
}
#[tokio::test]
async fn default_none_and_only_configuration_supplies_immutable_descriptor() {
    for version in ["3.0.3", "3.1.0"] {
        let mut source = document(version);
        let handler = SpecificationHandler::from_document(&source).unwrap();
        source["paths"]["/{specId}"]["get"]["operationId"] = json!("mutated-source");
        source["security"] = json!([{"jwt":["admin"]}]);
        let mut state = ExchangeState::default();
        assert!(state.operation.is_none());
        state.principal = Some(Principal::from_verified(
            "existing-principal".into(),
            None,
            None,
            json!({}),
        ));
        state.correlation_id = Some("existing-correlation".into());
        state.traceability_id = Some("existing-trace".into());
        state
            .response_headers
            .insert("x-existing", "keep".parse().unwrap());
        let mut inv = invocation();
        inv.path = "/private-caller-value".into();
        inv.original = json!({"operation":{"operationId":"forged","security":[{"jwt":["admin"]}]},"principal":"forged","route":{"template":"/forged"}});
        inv.headers
            .insert("x-operation-id", "forged".parse().unwrap());
        inv.route = Some(RouteMatch {
            template: "/handler/{handlerId}".into(),
            target: None,
        });
        inv.path_params.insert("existing".into(), "keep".into());
        assert!(matches!(
            handler.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Continue
        ));
        let operation = state.operation.as_ref().unwrap();
        assert_eq!(operation.operation_id(), Some("configured-operation"));
        assert_eq!(operation.path_template(), "/{specId}");
        assert_eq!(operation.method(), "GET");
        assert_eq!(
            operation.dialect(),
            if version == "3.0.3" {
                Dialect::OpenApi30
            } else {
                Dialect::OpenApi31
            }
        );
        assert_eq!(
            operation.security_requirements().unwrap()[0]["jwt"],
            vec!["read"]
        );
        assert_eq!(operation.document()["security"], json!([{"jwt":["read"]}]));
        assert_eq!(
            state.principal.as_ref().unwrap().principal_id(),
            "existing-principal"
        );
        assert_eq!(
            state.correlation_id.as_deref(),
            Some("existing-correlation")
        );
        assert_eq!(state.traceability_id.as_deref(), Some("existing-trace"));
        assert_eq!(state.response_headers["x-existing"], "keep");
        assert_eq!(inv.route.as_ref().unwrap().template, "/handler/{handlerId}");
        assert_eq!(inv.path_params.len(), 1);
        assert_eq!(inv.path_params["existing"], "keep");
        assert_eq!(inv.headers["x-operation-id"], "forged");
        assert_eq!(state.metrics.dispatches, 0);
        assert!(!state.metrics.finalized);
    }
}

#[tokio::test]
async fn path_selection_precedes_method_and_absence_clears_previous_metadata() {
    let handler = SpecificationHandler::from_document(&document("3.1.0")).unwrap();
    let mut state = ExchangeState::default();
    let mut inv = invocation();
    inv.path = "/overridden".into();
    handler.on_request(&mut inv, &mut state).await.unwrap();
    let requirement = state
        .operation
        .as_ref()
        .unwrap()
        .security_requirements()
        .unwrap();
    assert_eq!(requirement.len(), 2);
    assert_eq!(requirement[0]["jwt"], vec!["write"]);
    assert!(requirement[0]["key"].is_empty());
    assert!(requirement[1].is_empty());
    inv.path = "/literal".into();
    // The broader GET /{specId} must not win over literal POST /literal.
    let HandlerOutcome::Reject(error) = handler.on_request(&mut inv, &mut state).await.unwrap()
    else {
        panic!("method mismatch must reject");
    };
    assert_eq!(error.response().status, 405);
    assert!(state.operation.is_none());
    inv.method = http::Method::POST;
    handler.on_request(&mut inv, &mut state).await.unwrap();
    assert!(
        state
            .operation
            .as_ref()
            .unwrap()
            .security_requirements()
            .unwrap()
            .is_empty()
    );
    inv.path = "/missing/two-segments".into();
    let HandlerOutcome::Reject(error) = handler.on_request(&mut inv, &mut state).await.unwrap()
    else {
        panic!("unmatched path must reject");
    };
    assert_eq!(error.response().status, 404);
    assert!(state.operation.is_none());
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    operation: Option<String>,
    template: Option<String>,
    route: Option<String>,
}
struct Observe(Arc<Mutex<Vec<Seen>>>);
impl Finalizer for Observe {
    fn finalize(&self, inv: &Invocation, state: &ExchangeState, _: &GatewayResponse) {
        self.0.lock().unwrap().push(Seen {
            operation: state
                .operation
                .as_ref()
                .and_then(|op| op.operation_id())
                .map(String::from),
            template: state.operation.as_ref().map(|op| op.path_template().into()),
            route: inv.route.as_ref().map(|r| r.template.clone()),
        });
    }
}
fn pipeline(counts: &Arc<Counts>, seen: &Arc<Mutex<Vec<Seen>>>, deny_before: bool) -> Pipeline {
    let mut registry = registry();
    let source = document("3.0.3");
    registry.register(
        HandlerId::Specification,
        Arc::new(SpecificationHandler::from_document(&source).unwrap()),
    );
    install(
        &mut registry,
        HandlerId::Proxy,
        Binding::Dispatch(
            TargetRoutes::compile([("/{proxyId}@get".into(), Target::Function("FnTest".into()))])
                .unwrap(),
        ),
    );
    let mut ids = vec![
        (HandlerId::Specification, "spec"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    install(
        &mut registry,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    let mut sequence = vec!["spec", "proxy", "response"];
    if deny_before {
        ids.insert(0, (HandlerId::RequestHeader, "deny"));
        sequence.insert(0, "deny");
        install(
            &mut registry,
            HandlerId::RequestHeader,
            Binding::Request(
                Arc::new(Handler::new(
                    HandlerId::RequestHeader,
                    Action::Reject,
                    counts,
                )),
                RequestCapabilities::default(),
            ),
        );
    }
    Pipeline::new(
        compile(config(&ids, &sequence), &registry),
        vec![counts.clone(), Arc::new(Observe(seen.clone()))],
    )
}
#[tokio::test]
async fn real_pipeline_metadata_warm_isolation_initial_deadline_cancel_and_early_denial() {
    let counts = Arc::new(Counts::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let p = pipeline(&counts, &seen, false);
    for mode in ["match", "missing", "deadline", "cancel", "match-again"] {
        let mut inv = invocation();
        inv.path = if mode == "missing" {
            "/missing/two-segments"
        } else {
            "/PRIVATE-CALLER-VALUE"
        }
        .into();
        inv.original = json!({"operation":{"operationId":"forged"}});
        if mode == "deadline" {
            inv.deadline = tokio::time::Instant::now();
        }
        if mode == "cancel" {
            inv.cancel.cancel();
        }
        let response = p.execute(inv, &FakeBackend::new(&counts)).await;
        assert_eq!(
            response.status.as_u16(),
            match mode {
                "match" | "match-again" => 200,
                "missing" => 404,
                _ => 504,
            }
        );
    }
    let observations = seen.lock().unwrap().clone();
    assert_eq!(observations.len(), 5);
    for index in [0, 4] {
        assert_eq!(
            observations[index].operation.as_deref(),
            Some("configured-operation")
        );
        assert_eq!(observations[index].template.as_deref(), Some("/{specId}"));
        assert_eq!(observations[index].route.as_deref(), Some("/{proxyId}"));
    }
    for index in [1, 2, 3] {
        assert!(observations[index].operation.is_none());
        assert!(observations[index].template.is_none());
    }
    assert_eq!(n(&counts.backend), 2);
    assert_eq!(n(&counts.response_entries), 5);
    assert_eq!(n(&counts.finalized), 5);
    drop(observations);
    let denial_counts = Arc::new(Counts::default());
    let denial_seen = Arc::new(Mutex::new(Vec::new()));
    let denied = pipeline(&denial_counts, &denial_seen, true);
    denied
        .execute(invocation(), &FakeBackend::new(&denial_counts))
        .await;
    assert!(denial_seen.lock().unwrap()[0].operation.is_none());
    assert_eq!(n(&denial_counts.backend), 0);
    assert_eq!(n(&denial_counts.response_entries), 1);
    assert_eq!(n(&denial_counts.finalized), 1);
}

#[test]
fn metadata_constructor_rejects_bad_shapes_and_remote_or_cyclic_path_refs_locally() {
    for source in [
        json!({"openapi":"2.0.0","paths":{}}),
        json!({"openapi":"3.1.0","paths":{},"security":"bad"}),
        json!({"openapi":"3.1.0","paths":{},"security":[{"jwt":[7]}]}),
        json!({"openapi":"3.1.0","paths":{"/x":{"$ref":"https://SECRET-NO-DIAGNOSTIC@example.invalid/spec"}}}),
        json!({"openapi":"3.1.0","paths":{"/x":{"$ref":"#/paths/~1x"}}}),
        json!({"openapi":"3.1.0","paths":{"/x":{"get":{"operationId":7}}}}),
    ] {
        let error = SpecificationHandler::from_document(&source)
            .err()
            .expect("invalid metadata must reject");
        assert!(!error.to_string().contains("SECRET-NO-DIAGNOSTIC"));
    }
    let source = json!({"openapi":"3.1.0","paths":{"/x":{"$ref":"#/components/pathItems/shared"}},"components":{"pathItems":{"shared":{"get":{"operationId":"local-ref"}}}}});
    assert!(SpecificationHandler::from_document(&source).is_ok());
}

#[test]
fn operation_metadata_does_not_establish_identity_for_a_limiter() {
    let mut registry = registry();
    registry.register(
        HandlerId::Specification,
        Arc::new(SpecificationHandler::from_document(&document("3.0.3")).unwrap()),
    );
    let counts = Arc::new(Counts::default());
    install(
        &mut registry,
        HandlerId::Limit,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Limit, Action::Continue, &counts)),
            RequestCapabilities {
                establishes_verified_identity: false,
                requires_verified_identity: true,
            },
        ),
    );
    let result = ExecutionPlan::compile(
        config(
            &[
                (HandlerId::Specification, "spec"),
                (HandlerId::Limit, "limit"),
                (HandlerId::Proxy, "proxy"),
            ],
            &["spec", "limit", "proxy"],
        ),
        json!({}),
        &registry,
    );
    assert!(matches!(result, Err(InitError::IdentityOrdering)));
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.request_entries), 0);
}

#[tokio::test]
async fn specification_metadata_template_cannot_replace_the_admitted_metrics_route() {
    use gateway_policy::observation::{RecordKind, RecordSink};
    struct Records(Arc<Mutex<Vec<Value>>>);
    impl RecordSink for Records {
        fn try_emit(&self, kind: RecordKind, value: Value) {
            assert_eq!(kind, RecordKind::Emf);
            self.0.lock().unwrap().push(value);
        }
    }
    let template = format!("/{{{}}}", "specId".repeat(220));
    assert!(template.len() > 1024);
    let mut source = document("3.0.3");
    source["paths"] = json!({template.clone():{"get":{"operationId":"large-metadata-template"}}});
    let mut registry = registry();
    registry.register(
        HandlerId::Specification,
        Arc::new(SpecificationHandler::from_document(&source).unwrap()),
    );
    let routes =
        TargetRoutes::compile([("/{proxyId}@get".into(), Target::Function("FnTest".into()))])
            .unwrap();
    install(
        &mut registry,
        HandlerId::Proxy,
        Binding::Dispatch(routes.clone()),
    );
    let records = Arc::new(Mutex::new(Vec::new()));
    registry.register(
        HandlerId::Metrics,
        Arc::new(
            serverless_core::handlers::metrics::MetricsHandler::new(
                "test-only-gateway".into(),
                Arc::new(Records(records.clone())),
            )
            .unwrap()
            .with_routes(Arc::new(routes)),
        ),
    );
    let counts = Arc::new(Counts::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let p = Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Metrics, "metrics"),
                    (HandlerId::Specification, "spec"),
                    (HandlerId::Proxy, "proxy"),
                ],
                &["metrics", "spec", "proxy"],
            ),
            &registry,
        ),
        vec![counts.clone(), Arc::new(Observe(seen.clone()))],
    );
    let mut inv = invocation();
    inv.path = "/PRIVATE-CALLER-VALUE".into();
    assert_eq!(p.execute(inv, &FakeBackend::new(&counts)).await.status, 200);
    assert_eq!(
        seen.lock().unwrap()[0].template.as_deref(),
        Some(template.as_str())
    );
    assert_eq!(seen.lock().unwrap()[0].route.as_deref(), Some("/{proxyId}"));
    let data = records.lock().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["Route"], "/{proxyId}");
    assert_eq!(data[0]["Requests"], 1);
    assert_eq!(data[0]["Errors"], 0);
    assert!(!data[0].to_string().contains("PRIVATE-CALLER-VALUE"));
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.finalized), 1);
}
