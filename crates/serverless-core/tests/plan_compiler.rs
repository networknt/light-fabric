pub mod support;
use serde_json::json;
use serverless_core::{compat::COMPATIBILITY, plan::*, *};
use std::sync::Arc;
use support::*;

#[test]
fn startup_rejections_are_explicit() {
    let r = registry();
    let cases = vec![
        (
            json!({"handlers":["example.Unknown@x"],"defaultHandlers":["x"]}),
            InitError::UnknownClass("example.Unknown".into()),
        ),
        (
            config(&[(HandlerId::Proxy, "proxy")], &["absent"]),
            InitError::UnknownAlias("absent".into()),
        ),
        (
            config(&[(HandlerId::Swt, "swt")], &["swt"]),
            InitError::Deferred(HandlerId::Swt),
        ),
        (
            config(&[(HandlerId::Security, "security")], &["security"]),
            InitError::Unimplemented(HandlerId::Security),
        ),
        (json!({"defaultHandlers":[]}), InitError::MissingTerminal),
        (
            config(&[(HandlerId::Proxy, "proxy")], &["proxy", "proxy"]),
            InitError::InvalidTerminal,
        ),
        (
            json!({"chains":{"a":["b"],"b":["a"]}}),
            InitError::Cycle("a".into()),
        ),
    ];
    for (c, expected) in cases {
        assert_eq!(
            ExecutionPlan::compile(c, json!({}), &r).err(),
            Some(expected)
        );
    }
}
#[test]
fn all_malformed_inputs_and_normalized_duplicates_fail() {
    let r = registry();
    let good = config(&[(HandlerId::Proxy, "proxy")], &["proxy"]);
    let mut bad = vec![
        json!([]),
        json!({"enabled":false}),
        json!({"enabled":"true"}),
    ];
    for (key, val) in [
        ("auditOnError", json!(true)),
        ("auditStackTrace", json!(true)),
        ("unknown", json!(1)),
        ("basePath", json!(3)),
        ("handlers", json!([entry(HandlerId::Proxy, "a@b")])),
        ("chains", json!({"a":7})),
        ("additionalPaths", json!(7)),
    ] {
        let mut c = good.clone();
        c[key] = val;
        bad.push(c);
    }
    for (path, verb) in [
        ("x", "get"),
        ("/x/{", "get"),
        ("/x/*/a", "get"),
        ("/{id}/{id}", "get"),
        ("/x?z", "get"),
        ("/x", ""),
        ("/x", "GET @"),
    ] {
        let mut c = good.clone();
        c["paths"] = json!([{"path":path,"method":verb,"exec":["proxy"]}]);
        bad.push(c);
    }
    let mut source = good.clone();
    source["paths"] = json!([{"source":"example.Java","exec":["proxy"]}]);
    bad.push(source);
    for c in bad {
        assert!(ExecutionPlan::compile(c, json!({}), &r).is_err());
    }
    for (a, b) in [("/x", "/x"), ("/x/{id}", "/x/{name}")] {
        let mut c = good.clone();
        c["paths"] = json!([{"path":a,"method":"GET","exec":["proxy"]}]);
        c["additionalPaths"] = json!([{"path":b,"method":"get","exec":["proxy"]}]);
        assert!(matches!(
            ExecutionPlan::compile(c, json!({}), &r),
            Err(InitError::DuplicateRoute(_))
        ));
    }
    for mapping in ["/x", "/x@", "/x@get@other"] {
        assert!(TargetRoutes::compile([(mapping.into(), Target::Function("FnX".into()))]).is_err());
    }
    assert!(TargetRoutes::compile([("/x@get".into(), Target::Function("".into()))]).is_err());
}
#[test]
fn disabled_handlers_are_removed_but_unknowns_and_disabled_terminals_do_not_pass() {
    let mut r = registry();
    r.disable(HandlerId::Swt);
    r.disable(HandlerId::Security);
    let p = compile(
        config(
            &[
                (HandlerId::Swt, "s"),
                (HandlerId::Security, "a"),
                (HandlerId::Proxy, "p"),
            ],
            &["s", "a", "p"],
        ),
        &r,
    );
    assert_eq!(
        p.handler_ids(&invocation()),
        Some([HandlerId::Proxy].as_slice())
    );
    r.disable(HandlerId::Proxy);
    assert_eq!(
        ExecutionPlan::compile(config(&[(HandlerId::Proxy, "p")], &["p"]), json!({}), &r).err(),
        Some(InitError::MissingTerminal)
    );
}
#[test]
fn nested_additions_alias_replacement_chain_precedence_and_audit_ownership() {
    let c = json!({"handlers":[entry(HandlerId::Proxy,"old")],"additionalHandlers":entry(HandlerId::Proxy,"new"),"chains":{"outer":["inner"],"inner":["absent"]},"additionalChains":{"inner":"new"},"defaultHandlers":"outer","basePath":"/ignored"});
    let p = ExecutionPlan::compile(
        c,
        json!({"auditOnError":true,"auditStackTrace":true}),
        &registry(),
    )
    .unwrap();
    assert_eq!(
        p.audit(),
        AuditConfig {
            audit_on_error: true,
            audit_stack_trace: true
        }
    );
    assert_eq!(p.base_path(), Some("/ignored"));
    assert_eq!(
        p.handler_ids(&invocation()),
        Some([HandlerId::Proxy].as_slice())
    );
    let c = json!({"handlers":[entry(HandlerId::Proxy,"p")],"chains":{"p":["q"]},"additionalHandlers":[entry(HandlerId::Proxy,"q")],"defaultHandlers":["p"]});
    assert!(ExecutionPlan::compile(c, json!({}), &registry()).is_ok());
    assert!(
        ExecutionPlan::compile(
            config(&[(HandlerId::Proxy, "p")], &["p"]),
            json!({"handler.auditOnError":true}),
            &registry()
        )
        .is_err()
    );
}
#[test]
fn effective_flattened_identity_order_not_alias_spelling() {
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Security,
        Binding::Request(
            Arc::new(Handler::new(
                HandlerId::Security,
                Action::Authenticate,
                &counts,
            )),
            RequestCapabilities {
                establishes_verified_identity: true,
                requires_verified_identity: false,
            },
        ),
    );
    install(
        &mut r,
        HandlerId::Limit,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Limit, Action::Continue, &counts)),
            RequestCapabilities {
                establishes_verified_identity: false,
                requires_verified_identity: true,
            },
        ),
    );
    let mut c = config(
        &[
            (HandlerId::Security, "not-the-security-alias"),
            (HandlerId::Limit, "limit"),
            (HandlerId::Proxy, "proxy"),
        ],
        &["outer", "proxy"],
    );
    c["chains"] = json!({"outer":["inner","limit"],"inner":["not-the-security-alias"]});
    assert!(ExecutionPlan::compile(c.clone(), json!({}), &r).is_ok());
    c["paths"] = json!([{"path":"/unsafe","method":"get","exec":["limit","inner","proxy"]}]);
    assert_eq!(
        ExecutionPlan::compile(c.clone(), json!({}), &r).err(),
        Some(InitError::IdentityOrdering)
    );
    c.as_object_mut().unwrap().remove("paths");
    r.disable(HandlerId::Security);
    assert_eq!(
        ExecutionPlan::compile(c.clone(), json!({}), &r).err(),
        Some(InitError::IdentityOrdering)
    );
    install(
        &mut r,
        HandlerId::Security,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Security, Action::Continue, &counts)),
            RequestCapabilities::default(),
        ),
    );
    assert_eq!(
        ExecutionPlan::compile(c, json!({}), &r).err(),
        Some(InitError::IdentityOrdering)
    );
}
#[test]
fn phase_arrangements_and_factory_identity_are_checked() {
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Correlation,
        Binding::Request(
            Arc::new(Handler::new(
                HandlerId::Correlation,
                Action::Continue,
                &counts,
            )),
            RequestCapabilities::default(),
        ),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    install(
        &mut r,
        HandlerId::Health,
        Binding::Local(Arc::new(Handler::new(
            HandlerId::Health,
            Action::Respond,
            &counts,
        ))),
    );
    let ids = [
        (HandlerId::Proxy, "p"),
        (HandlerId::Correlation, "q"),
        (HandlerId::ResponseHeader, "r"),
        (HandlerId::Health, "l"),
    ];
    for exec in [
        &["p", "q"][..],
        &["r", "p"][..],
        &["l", "p"][..],
        &["p", "l"][..],
    ] {
        assert_eq!(
            ExecutionPlan::compile(config(&ids, exec), json!({}), &r).err(),
            Some(InitError::InvalidTerminal)
        );
    }
    install(
        &mut r,
        HandlerId::Correlation,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Jwt, Action::Continue, &counts)),
            RequestCapabilities::default(),
        ),
    );
    assert_eq!(
        ExecutionPlan::compile(config(&ids, &["q", "p"]), json!({}), &r).err(),
        Some(InitError::FactoryContract(HandlerId::Correlation))
    );
}
#[test]
fn route_precedence_and_path_copies_follow_wp2_and_r01a() {
    let routes = TargetRoutes::compile([
        ("/pets/{id}/*".into(), Target::Function("Wildcard".into())),
        (
            "/pets/{id}/toys/{toy}@get".into(),
            Target::Function("Longest".into()),
        ),
    ]);
    assert!(routes.is_err()); // malformed mapping cannot be silently skipped
    let routes = TargetRoutes::compile([
        (
            "/pets/{id}/*@get".into(),
            Target::Function("Wildcard".into()),
        ),
        (
            "/pets/{id}/toys/{toy}@GET".into(),
            Target::Function("Longest".into()),
        ),
        ("/pets/fixed@get".into(), Target::Function("Literal".into())),
        ("/pets/{id}@get".into(), Target::Function("Template".into())),
    ])
    .unwrap();
    for (path, expected) in [
        ("/pets/fixed", "Literal"),
        ("/pets/42", "Template"),
        ("/pets/42/toys/7", "Longest"),
        ("/pets/42/other", "Wildcard"),
    ] {
        let mut inv = invocation();
        inv.path = path.into();
        inv.query = MultiMap(vec![
            ("id".into(), "original".into()),
            ("other".into(), "1".into()),
            ("other".into(), "2".into()),
        ]);
        assert_eq!(
            routes.resolve(&mut inv),
            Some(Target::Function(expected.into()))
        );
        assert_eq!(
            inv.query.0.iter().find(|(k, _)| k == "id").unwrap().1,
            "original"
        );
        assert_eq!(inv.query.0.iter().filter(|(k, _)| k == "other").count(), 2);
        if expected == "Longest" {
            assert_eq!(inv.path_params.get("toy").unwrap(), "7");
            assert!(!inv.path_params.contains_key("*"));
        }
    }
    let mut inv = invocation();
    inv.path = "/pets/%252F".into();
    assert_eq!(
        routes.resolve(&mut inv),
        Some(Target::Function("Template".into()))
    );
    assert_eq!(inv.path_params["id"], "%252F");
    inv.method = http::Method::POST;
    assert!(routes.resolve(&mut inv).is_none());
}
#[test]
fn compatibility_table_covers_approved_classes_without_implicit_factories() {
    assert_eq!(COMPATIBILITY.len(), 28);
    for c in COMPATIBILITY {
        assert_eq!(serverless_core::compat::lookup(c.class).unwrap().id, c.id);
        assert!(
            ExecutionPlan::compile(
                config(&[(c.id, "h")], &["h"]),
                json!({}),
                &Registry::default()
            )
            .is_err()
        );
    }
}

#[test]
fn identity_key_and_jwt_resolver_classification_drives_real_compiler() {
    let counts = Arc::new(Counts::default());
    for (key, resolver, needs_identity) in [
        ("client", None, true),
        ("user", None, true),
        ("jwtClaim", None, true),
        (
            "server",
            Some(
                "com.networknt.aws.lambda.handler.middleware.limit.key.JwtHeaderClientIdKeyResolver",
            ),
            true,
        ),
        ("address", None, false),
        ("server", None, false),
        ("instance", None, false),
    ] {
        let mut r = registry();
        let caps = RequestCapabilities::for_limit(key, resolver).unwrap();
        install(
            &mut r,
            HandlerId::Limit,
            Binding::Request(
                Arc::new(Handler::new(HandlerId::Limit, Action::Continue, &counts)),
                caps,
            ),
        );
        let result = ExecutionPlan::compile(
            config(
                &[(HandlerId::Limit, "l"), (HandlerId::Proxy, "p")],
                &["l", "p"],
            ),
            json!({}),
            &r,
        );
        if needs_identity {
            assert_eq!(result.err(), Some(InitError::IdentityOrdering));
        } else {
            assert!(result.is_ok());
        }
    }
    assert!(RequestCapabilities::for_limit("custom", None).is_err());
}

#[test]
fn handler_path_matchers_prioritize_literals_longest_and_methods() {
    let c = Arc::new(Counts::default());
    let mut r = registry();
    for id in [HandlerId::Health, HandlerId::Info, HandlerId::GetLogger] {
        install(
            &mut r,
            id,
            Binding::Local(Arc::new(Handler::new(id, Action::Respond, &c))),
        );
    }
    let mut cfg = config(
        &[
            (HandlerId::Proxy, "p"),
            (HandlerId::Health, "h"),
            (HandlerId::Info, "i"),
            (HandlerId::GetLogger, "l"),
        ],
        &["p"],
    );
    cfg["paths"] = json!([
        {"path":"/pets/{id}/*","method":"get","exec":["h"]},
        {"path":"/pets/{id}/toys/{toy}","method":"get","exec":["i"]},
        {"path":"/pets/fixed/toys/7","method":"get","exec":["l"]}
    ]);
    let p = compile(cfg, &r);
    for (path, id) in [
        ("/pets/42/other", HandlerId::Health),
        ("/pets/42/toys/7", HandlerId::Info),
        ("/pets/fixed/toys/7", HandlerId::GetLogger),
        ("/unknown", HandlerId::Proxy),
    ] {
        let mut inv = invocation();
        inv.path = path.into();
        assert_eq!(p.handler_ids(&inv), Some([id].as_slice()));
        inv.method = http::Method::POST;
        assert_eq!(p.handler_ids(&inv), Some([HandlerId::Proxy].as_slice()));
    }
}

struct CountingFactory(Arc<std::sync::atomic::AtomicUsize>);
impl HandlerFactory for CountingFactory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Binding::Dispatch(targets()))
    }
}
#[tokio::test]
async fn factories_construct_once_at_init_and_compiled_plans_are_immutable() {
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut r = Registry::default();
    r.register(HandlerId::Proxy, Arc::new(CountingFactory(builds.clone())));
    let cfg = config(&[(HandlerId::Proxy, "p")], &["p"]);
    let plan = compile(cfg, &r);
    assert_eq!(n(&builds), 1);
    r.disable(HandlerId::Proxy);
    let c = Arc::new(Counts::default());
    let pipeline = Pipeline::new(plan, vec![c.clone()]);
    for _ in 0..2 {
        assert_eq!(
            pipeline
                .execute(invocation(), &FakeBackend::new(&c))
                .await
                .status,
            200
        );
    }
    assert_eq!(n(&builds), 1);
    assert_eq!(n(&c.backend), 2);
}

#[test]
fn explicit_influx_selection_remains_deferred_even_with_a_metrics_factory() {
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Metrics,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Metrics, Action::Continue, &counts)),
            RequestCapabilities::default(),
        ),
    );
    let c = json!({"handlers":["com.networknt.aws.lambda.handler.middleware.metrics.InfluxMetricsMiddleware@metrics",entry(HandlerId::Proxy,"p")],"defaultHandlers":["metrics","p"]});
    assert_eq!(
        ExecutionPlan::compile(c.clone(), json!({}), &r).err(),
        Some(InitError::Deferred(HandlerId::Metrics))
    );
    r.disable(HandlerId::Metrics);
    assert!(ExecutionPlan::compile(c, json!({}), &r).is_ok());
}
