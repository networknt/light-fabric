pub mod support;
use gateway_policy::openapi_operation::SourceFormat;
use serde_json::{Value, json};
use serverless_core::{handlers::specification::SpecificationHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;

struct Seen(Arc<Mutex<Vec<(u16, bool, String)>>>);
impl Finalizer for Seen {
    fn finalize(&self, inv: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        self.0.lock().unwrap().push((
            response.status.as_u16(),
            state.operation.is_some(),
            inv.path.clone(),
        ));
    }
}
#[tokio::test]
async fn utf8_boundaries_no_dispatch_privacy_and_once_only_pipeline_outcomes() {
    let handler = SpecificationHandler::from_source(
        br#"{"openapi":"3.1.0","paths":{"/{id}":{"get":{"operationId":"trusted"}}}}"#,
        SourceFormat::Json,
    )
    .unwrap();
    let mut registry = registry();
    registry.register(HandlerId::Specification, Arc::new(handler));
    let counts = Arc::new(Counts::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    install(
        &mut registry,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    let pipeline = Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Specification, "spec"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseHeader, "response"),
                ],
                &["spec", "proxy", "response"],
            ),
            &registry,
        ),
        vec![counts.clone(), Arc::new(Seen(seen.clone()))],
    );
    let inputs = [
        ("/".to_string() + &"x".repeat(8191), 200, "normal"),
        ("/".to_string() + &"x".repeat(8192), 414, "oversize"),
        ("/".to_string() + &"é".repeat(4095) + "x", 200, "utf8"),
        ("/".to_string() + &"é".repeat(4096), 414, "utf8-oversize"),
        ("/".to_string() + &"x".repeat(8192), 504, "expired"),
        ("/".to_string() + &"x".repeat(8192), 504, "cancelled"),
        ("/warm".to_string(), 200, "warm"),
        (
            "/".to_string() + &"x".repeat(1_048_575),
            414,
            "amended-legacy-1MiB",
        ),
    ];
    for (i, (path, status, mode)) in inputs.iter().enumerate() {
        let mut inv = invocation();
        inv.path = path.clone();
        inv.raw_path = path.clone();
        if *mode == "expired" {
            inv.deadline = tokio::time::Instant::now();
        }
        if *mode == "cancelled" {
            inv.cancel.cancel();
        }
        let backend_before = n(&counts.backend);
        let response = pipeline.execute(inv, &FakeBackend::new(&counts)).await;
        assert_eq!(response.status.as_u16(), *status, "{mode}");
        assert_eq!(
            n(&counts.backend),
            backend_before + usize::from(*status == 200)
        );
        assert_eq!(n(&counts.response_entries), i + 1);
        assert_eq!(n(&counts.finalized), i + 1);
        let rows = seen.lock().unwrap();
        assert_eq!(rows[i].1, *status == 200);
        assert_eq!(&rows[i].2, path);
        if *status == 414 {
            let body: Value = serde_json::from_slice(response.body.as_ref().unwrap()).unwrap();
            assert_eq!(body["code"], "ERR14029");
            assert_eq!(body["message"], "SPECIFICATION_PATH_TOO_LARGE");
            assert_eq!(
                body["description"],
                "Request path exceeds the supported Specification size."
            );
            assert!(response.body.as_ref().unwrap().len() < 256);
        }
    }
}

#[tokio::test]
async fn raw_json_yaml_first_server_fallback_and_matching_view_isolation() {
    let sources=[(br#"{"openapi":"3.0.3","servers":[{"url":"https://never-contact.invalid/api"}],"paths":{"/pets":{"get":{"operationId":"trusted"}}}}"#.as_slice(),SourceFormat::Json),(b"openapi: 3.1.0\nservers: [{url: '/api'}]\npaths: {'/pets': {get: {operationId: trusted}}}\n".as_slice(),SourceFormat::Yaml)];
    for (source, format) in sources {
        let h = SpecificationHandler::from_source_with_base_path(
            source,
            format,
            Some(&json!("(?=unused-unsupported)")),
        )
        .unwrap();
        for (path, status) in [
            ("/api/pets", 200),
            ("/prefix/api/pets", 404),
            ("/api/unknown", 404),
            ("/api/pets", 200),
        ] {
            let mut inv = invocation();
            inv.path = path.into();
            inv.raw_path = path.into();
            inv.route = Some(RouteMatch {
                template: "/api/{trusted-handler-route}".into(),
                target: None,
            });
            let mut state = ExchangeState::default();
            state.principal = Some(Principal::from_verified(
                "preserved".into(),
                None,
                None,
                json!({}),
            ));
            let result = h.on_request(&mut inv, &mut state).await.unwrap();
            assert_eq!(matches!(result, HandlerOutcome::Continue), status == 200);
            assert_eq!(inv.path, path);
            assert_eq!(inv.raw_path, path);
            assert_eq!(
                inv.route.as_ref().unwrap().template,
                "/api/{trusted-handler-route}"
            );
            assert_eq!(
                state.principal.as_ref().unwrap().principal_id(),
                "preserved"
            );
            assert_eq!(state.operation.is_some(), status == 200);
            if status == 200 {
                assert_eq!(
                    state.operation.as_ref().unwrap().operation_id(),
                    Some("trusted")
                );
            }
        }
    }
    let source=br#"{"openapi":"3.1.0","servers":[{"url":"https://never-contact.invalid"}],"paths":{"/pets":{"get":{}}}}"#;
    let h = SpecificationHandler::from_source_with_base_path(
        source,
        SourceFormat::Json,
        Some(&json!("/v1.0")),
    )
    .unwrap();
    let mut inv = invocation();
    inv.path = "/v1x0/pets".into();
    assert!(matches!(
        h.on_request(&mut inv, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Continue
    ));
}
