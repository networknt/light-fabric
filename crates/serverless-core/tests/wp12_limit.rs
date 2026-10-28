pub mod support;
use serverless_core::{handlers::limit::LimitHandler, plan::*, *};
use std::sync::Arc;
use support::*;
fn handler(yaml: &str) -> LimitHandler {
    LimitHandler::from_yaml(serde_yaml::from_str(yaml).unwrap()).unwrap()
}
fn pipeline_limit(yaml: &str, c: &Arc<Counts>) -> Pipeline {
    let mut r = registry();
    r.register(HandlerId::Limit, Arc::new(handler(yaml)));
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            c,
        ))),
    );
    Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Limit, "limit"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseHeader, "response"),
                ],
                &["limit", "proxy", "response"],
            ),
            &r,
        ),
        vec![c.clone()],
    )
}
#[tokio::test]
async fn approved_server_fixture_first_quota_envelope_body_headers_and_counts() {
    let c = Arc::new(Counts::default());
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/fixtures/v1/l/L-01-server-exhaustion-multi-v1.json"
    ))
    .unwrap();
    let p = pipeline_limit(
        &serde_yaml::to_string(&fixture["config"]["files"]["limit"]).unwrap(),
        &c,
    );
    for i in 0..11 {
        let response = p.execute(invocation(), &FakeBackend::new(&c)).await;
        assert_eq!(
            response.status.as_u16(),
            fixture["steps"][i]["expect"]["status"].as_u64().unwrap() as u16
        );
        if i == 10 {
            assert_eq!(
                response.body.as_ref().unwrap().as_ref(),
                fixture["steps"][i]["expect"]["body"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
            );
            assert_eq!(response.headers["ratelimit-limit"], "10/MINUTES");
            assert_eq!(response.headers["ratelimit-remaining"], "0");
            let reset = response.headers["ratelimit-reset"]
                .to_str()
                .unwrap()
                .strip_suffix('s')
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!((1..=60).contains(&reset));
            assert!(!response.headers.contains_key("retry-after"));
        } else {
            assert!(!response.headers.contains_key("ratelimit-limit"));
        }
        assert!(response.headers.contains_key("x-response-policy"));
    }
    assert_eq!(n(&c.backend), 10);
    assert_eq!(n(&c.response_entries), 11);
    assert_eq!(n(&c.finalized), 11);
}
#[tokio::test]
async fn configured_address_retains_state_fallback_recreates_and_paths_isolate() {
    let h =
        handler("enabled: true\nkey: address\nrateLimit: 1/m\naddress: {'203.0.113.7': '1/m'}\n");
    for (address, expected) in [
        ("203.0.113.7", true),
        ("203.0.113.7", false),
        ("198.51.100.2", true),
        ("198.51.100.2", true),
    ] {
        let mut inv = invocation();
        inv.source.source_ip = Some(address.parse().unwrap());
        let result = h
            .on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap();
        assert_eq!(matches!(result, HandlerOutcome::Continue), expected);
    }
    let c = Arc::new(Counts::default());
    let p = pipeline_limit("enabled: true\nrateLimit: 1/m\nserver: {'/a': '2/m'}\n", &c);
    for path in ["/b", "/c", "/a/one", "/a/two"] {
        let mut inv = invocation();
        inv.path = path.into();
        assert_eq!(p.execute(inv, &FakeBackend::new(&c)).await.status, 200);
    }
    let mut inv = invocation();
    inv.path = "/a/three".into();
    assert_eq!(p.execute(inv, &FakeBackend::new(&c)).await.status, 429);
}
#[tokio::test]
async fn identity_labels_use_verified_state_and_do_not_consume_on_missing_identity() {
    for key in ["client", "user"] {
        let h = handler(&format!(
            "enabled: true\nkey: {key}\nrateLimit: 1/m\n{key}: {{verified: '1/m'}}\n"
        ));
        let mut inv = invocation();
        inv.headers.insert("client-id", "verified".parse().unwrap());
        assert!(matches!(
            h.on_request(&mut inv, &mut ExchangeState::default())
                .await
                .unwrap(),
            HandlerOutcome::Reject(_)
        ));
        for (id, allow) in [
            ("verified", true),
            ("verified", false),
            ("other", true),
            ("other", true),
        ] {
            let mut state = ExchangeState::default();
            state.principal = Some(Principal::from_verified(
                id.into(),
                Some(id.into()),
                Some(id.into()),
                serde_json::json!({}),
            ));
            assert_eq!(
                matches!(
                    h.on_request(&mut inv, &mut state).await.unwrap(),
                    HandlerOutcome::Continue
                ),
                allow
            );
        }
    }
}
#[test]
fn real_factory_rejects_identity_before_security_and_unknown_config() {
    let mut registry = registry();
    registry.register(
        HandlerId::Limit,
        Arc::new(handler("enabled: true\nkey: client\nrateLimit: 1/m\n")),
    );
    assert!(
        ExecutionPlan::compile(
            config(
                &[(HandlerId::Limit, "limit"), (HandlerId::Proxy, "proxy")],
                &["limit", "proxy"]
            ),
            serde_json::json!({}),
            &registry
        )
        .is_err()
    );
    for yaml in [
        "enabled: false\nheadersAlwaysSet: true",
        "enabled: false\nconcurrentRequest: 9",
        "queueSize: 0",
        "extra: true",
        "enabled: true\nrateLimit: bad",
        "key: 2",
        "key: other",
        "clientIdKeyResolver: arbitrary.Plugin",
    ] {
        assert!(
            LimitHandler::from_yaml(serde_yaml::from_str(yaml).unwrap()).is_err(),
            "{yaml}"
        );
    }
    let warnings =
        LimitHandler::warnings(&serde_yaml::from_str("enabled: false").unwrap()).unwrap();
    assert_eq!(warnings.len(), 3);
}

#[tokio::test]
async fn dynamic_server_prefix_buckets_and_java_hash_iteration_are_preserved() {
    let c = Arc::new(Counts::default());
    let p = pipeline_limit("enabled: true\nrateLimit: 1/m\n", &c);
    for (path, status) in [("/x", 200), ("/x/child", 429), ("/xy", 429), ("/a", 200)] {
        let mut inv = invocation();
        inv.path = path.into();
        assert_eq!(p.execute(inv, &FakeBackend::new(&c)).await.status, status);
    }
    assert_eq!(n(&c.backend), 2);
    assert_eq!(n(&c.response_entries), 4);
    assert_eq!(n(&c.finalized), 4);
    // Java's hash-table bucket order selects /x before /, even when declared second.
    let c = Arc::new(Counts::default());
    let p = pipeline_limit(
        "enabled: true\nrateLimit: 9/m\nserver: {'/': '1/m', '/x': '2/m'}\n",
        &c,
    );
    for (path, status) in [
        ("/x/one", 200),
        ("/x/two", 200),
        ("/x/three", 429),
        ("/other", 200),
        ("/else", 429),
    ] {
        let mut inv = invocation();
        inv.path = path.into();
        assert_eq!(p.execute(inv, &FakeBackend::new(&c)).await.status, status);
    }
    assert_eq!(n(&c.backend), 3);
    assert_eq!(n(&c.finalized), 5);
}
#[tokio::test]
async fn zero_quota_allows_first_empty_bucket_and_direct_first_quota_wins() {
    for yaml in [
        "enabled: true\nrateLimit: 0/s\n",
        "enabled: true\nkey: address\nrateLimit: 7/s\naddress: {'203.0.113.7': '0/s 10/s'}\n",
    ] {
        let c = Arc::new(Counts::default());
        let p = pipeline_limit(yaml, &c);
        let mut inv = invocation();
        inv.source.source_ip = Some("203.0.113.7".parse().unwrap());
        assert_eq!(
            p.execute(inv.clone(), &FakeBackend::new(&c)).await.status,
            200
        );
        let denied = p.execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(denied.status, 429);
        assert_eq!(denied.headers["ratelimit-limit"], "0/SECONDS");
        assert_eq!(denied.headers["ratelimit-remaining"], "-1");
        assert_eq!(denied.headers["ratelimit-reset"], "1s");
        assert_eq!(n(&c.backend), 1);
        assert_eq!(n(&c.finalized), 2);
    }
    for quotas in ["1/m 10/m", "1/m 10/d"] {
        let c = Arc::new(Counts::default());
        let p = pipeline_limit(
            &format!(
                "enabled: true\nkey: address\nrateLimit: 9/m\naddress: {{'203.0.113.7': '{quotas}'}}\n"
            ),
            &c,
        );
        let mut inv = invocation();
        inv.source.source_ip = Some("203.0.113.7".parse().unwrap());
        assert_eq!(
            p.execute(inv.clone(), &FakeBackend::new(&c)).await.status,
            200
        );
        assert_eq!(p.execute(inv, &FakeBackend::new(&c)).await.status, 429);
    }
}
#[tokio::test]
async fn source_resolvers_require_explicit_metadata_and_verified_identity() {
    let h = handler("enabled: true\nkey: address\nrateLimit: 1/m\n");
    let mut inv = invocation();
    inv.headers
        .insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
    assert!(matches!(
        h.on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Reject(_)
    ));
    let h = handler(
        "enabled: true\nkey: address\naddressKeyResolver: com.networknt.limit.key.AkamaiAddressKeyResolver\nrateLimit: 1/m\naddress: {'test-only-label': '1/m'}\n",
    );
    inv.headers
        .insert("true-client-ip", "test-only-label".parse().unwrap());
    assert!(matches!(
        h.on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Continue
    ));
    assert!(matches!(
        h.on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Respond(_)
    ));
    let h = handler(
        "enabled: true\nkey: client\nclientIdKeyResolver: com.networknt.limit.key.JwtHeaderClientIdKeyResolver\nrateLimit: 1/m\nclient: {'test-only-header': '1/m'}\n",
    );
    inv.headers
        .insert("client-id", "test-only-header".parse().unwrap());
    assert!(matches!(
        h.on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Reject(_)
    ));
    let mut state = ExchangeState::default();
    state.principal = Some(Principal::from_verified(
        "test-only".into(),
        None,
        None,
        serde_json::json!({}),
    ));
    for label in ["test-only-header", "another-header"] {
        inv.headers.insert("client-id", label.parse().unwrap());
        assert!(matches!(
            h.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Reject(_)
        ));
    }
    state.principal = Some(Principal::from_verified(
        "test-only".into(),
        Some("test-only-header".into()),
        None,
        serde_json::json!({}),
    ));
    assert!(matches!(
        h.on_request(&mut inv, &mut state).await.unwrap(),
        HandlerOutcome::Continue
    ));
    inv.headers
        .insert("client-id", "yet-another-header".parse().unwrap());
    assert!(matches!(
        h.on_request(&mut inv, &mut state).await.unwrap(),
        HandlerOutcome::Respond(_)
    ));
    assert_eq!(
        state.principal.as_ref().unwrap().client_id(),
        Some("test-only-header")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_new_server_bucket_retains_one_shared_quota_and_once_only_counts() {
    let counts = Arc::new(Counts::default());
    let pipeline = Arc::new(pipeline_limit("enabled: true\nrateLimit: 10/m\n", &counts));
    let barrier = Arc::new(tokio::sync::Barrier::new(64));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..64 {
        let pipeline = pipeline.clone();
        let barrier = barrier.clone();
        let counts = counts.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            pipeline
                .execute(invocation(), &FakeBackend::new(&counts))
                .await
                .status
        });
    }
    let mut successes = 0;
    let mut denials = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap().as_u16() {
            200 => successes += 1,
            429 => denials += 1,
            status => panic!("unexpected status {status}"),
        }
    }
    assert_eq!((successes, denials), (10, 54));
    assert_eq!(n(&counts.backend), 10);
    assert_eq!(n(&counts.response_entries), 64);
    assert_eq!(n(&counts.finalized), 64);
}

#[tokio::test]
async fn configured_server_collision_tree_preserves_source_map_bucket_selection() {
    // Actual source-compiled Java limiter with hash-verified WP2 dependencies
    // first inserts quota HashMap keys into the counter ConcurrentHashMap.
    // Longest, /, then /0p therefore allow, allow, deny. The standalone probe's
    // earlier second-request denial was disproved; its failure is retained.
    let counts = Arc::new(Counts::default());
    let pipeline = pipeline_limit(
        "enabled: true\nrateLimit: 1/m\nserver:\n  \"/0p0C0o0L7a1b3c2j2j1p3c\": 1/m\n  \"/0p0C0o0L7a1b3c2j2j1p\": 1/m\n  \"/0p0C0o0L7a1b3c2j2j\": 1/m\n  \"/0p0C0o0L7a1b3c2j\": 1/m\n  \"/0p0C0o0L7a1b3c\": 1/m\n  \"/0p0C0o0L7a1b\": 1/m\n  \"/0p0C0o0L7a\": 1/m\n  \"/0p0C0o0L\": 1/m\n  \"/0p0C0o\": 1/m\n  \"/0p0C\": 1/m\n  \"/0p\": 1/m\n  \"/\": 1/m\n",
        &counts,
    );
    let mut inv = invocation();
    inv.path = "/0p0C0o0L7a1b3c2j2j1p3c".into();
    assert_eq!(
        pipeline
            .execute(inv, &FakeBackend::new(&counts))
            .await
            .status,
        200
    );
    let mut inv = invocation();
    inv.path = "/".into();
    let response = pipeline.execute(inv, &FakeBackend::new(&counts)).await;
    assert_eq!(n(&counts.response_entries), 2);
    assert_eq!(n(&counts.finalized), 2);
    assert_eq!(response.status, 200);
    assert_eq!(n(&counts.backend), 2);
    let mut inv = invocation();
    inv.path = "/0p".into();
    let response = pipeline.execute(inv, &FakeBackend::new(&counts)).await;
    assert_eq!(
        response.status, 429,
        "actual Java traversal reuses /0p bucket"
    );
    assert_eq!(n(&counts.backend), 2);
    assert_eq!(n(&counts.response_entries), 3);
    assert_eq!(n(&counts.finalized), 3);
}

#[tokio::test]
async fn actual_java25_request_traces_reach_real_limiter_pipeline_with_exact_counts() {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("data/wp12_limit_java25_traversal.json")).unwrap();
    for case in data["cases"].as_array().unwrap() {
        let counts = Arc::new(Counts::default());
        // Serialize declarations from the ordered vector, not serde_json's
        // sorted map, so the constructor receives the qualified source order.
        let mut yaml = "enabled: true\nrateLimit: 1/h\nserver:\n".to_owned();
        if case["declared"].as_array().unwrap().is_empty() {
            yaml.push_str("  {}\n");
        } else {
            for key in case["declared"].as_array().unwrap() {
                yaml.push_str(&format!("  {}: 1/h\n", serde_json::to_string(key).unwrap()));
            }
        }
        if case["name"] == "nested-string" {
            let entries = case["declared"]
                .as_array()
                .unwrap()
                .iter()
                .map(|key| format!("{}:\"1/h\"", serde_json::to_string(key).unwrap()))
                .collect::<Vec<_>>()
                .join(",");
            yaml = format!(
                "enabled: true\nrateLimit: 1/h\nserver: {}\n",
                serde_json::to_string(&format!("{{{entries}}}")).unwrap()
            );
        }
        let pipeline = pipeline_limit(&yaml, &counts);
        let mut dispatches = 0;
        let steps = case["steps"].as_array().unwrap();
        for step in steps {
            let mut inv = invocation();
            inv.path = step["path"].as_str().unwrap().into();
            let allowed = step["allow"].as_bool().unwrap();
            dispatches += usize::from(allowed);
            let response = pipeline.execute(inv, &FakeBackend::new(&counts)).await;
            assert_eq!(
                response.status.as_u16(),
                if allowed { 200 } else { 429 },
                "{} {}",
                case["name"],
                step["path"]
            );
        }
        assert_eq!(n(&counts.backend), dispatches);
        assert_eq!(n(&counts.response_entries), steps.len());
        assert_eq!(n(&counts.finalized), steps.len());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_different_buckets_grow_then_deny_warm_repeats_without_cross_consumption() {
    let counts = Arc::new(Counts::default());
    let pipeline = Arc::new(pipeline_limit("enabled: true\nrateLimit: 1/h\n", &counts));
    for repeat in [false, true] {
        let barrier = Arc::new(tokio::sync::Barrier::new(64));
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..64 {
            let pipeline = pipeline.clone();
            let barrier = barrier.clone();
            let counts = counts.clone();
            tasks.spawn(async move {
                let mut inv = invocation();
                inv.path = format!("/many/{i:03}/leaf");
                barrier.wait().await;
                pipeline
                    .execute(inv, &FakeBackend::new(&counts))
                    .await
                    .status
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.unwrap().as_u16(), if repeat { 429 } else { 200 });
        }
    }
    assert_eq!(n(&counts.backend), 64);
    assert_eq!(n(&counts.response_entries), 128);
    assert_eq!(n(&counts.finalized), 128);
    let fresh = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.path = "/many/000/leaf".into();
    let response = pipeline_limit("enabled: true\nrateLimit: 1/h\n", &fresh)
        .execute(inv, &FakeBackend::new(&fresh))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(n(&fresh.backend), 1);
    assert_eq!(n(&fresh.finalized), 1);
}
