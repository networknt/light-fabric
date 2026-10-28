pub mod support;
use gateway_policy::observation::{RecordKind, RecordSink};
use serde_json::Value;
use serverless_core::{handlers::metrics::MetricsHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;
#[derive(Default)]
struct Records(Mutex<Vec<Value>>);
impl RecordSink for Records {
    fn try_emit(&self, kind: RecordKind, value: Value) {
        assert_eq!(kind, RecordKind::Emf);
        self.0.lock().unwrap().push(value);
    }
}
fn setup(records: &Arc<Records>, counts: &Arc<Counts>) -> Pipeline {
    setup_sink(records.clone(), counts, false)
}
fn setup_sink(sink: Arc<dyn RecordSink>, counts: &Arc<Counts>, deny: bool) -> Pipeline {
    let mut r = registry();
    let routes = TargetRoutes::compile([(
        "/configured/{id}@get".into(),
        Target::Function("FnTest".into()),
    )])
    .unwrap();
    install(&mut r, HandlerId::Proxy, Binding::Dispatch(routes.clone()));
    r.register(
        HandlerId::Metrics,
        Arc::new(
            MetricsHandler::new("test-only-gateway".into(), sink)
                .unwrap()
                .with_routes(Arc::new(routes)),
        ),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    let mut ids = vec![
        (HandlerId::Metrics, "metrics"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let mut sequence = vec!["metrics", "proxy", "response"];
    if deny {
        install(
            &mut r,
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
        ids.insert(0, (HandlerId::RequestHeader, "deny"));
        sequence.insert(0, "deny");
    }
    Pipeline::new(compile(config(&ids, &sequence), &r), vec![counts.clone()])
}

#[test]
fn read_only_route_matching_preserves_literal_template_wildcard_and_method_precedence() {
    let routes = TargetRoutes::compile([
        ("/*@get".into(), Target::Function("catchall".into())),
        ("/configured/*@get".into(), Target::Function("wild".into())),
        (
            "/configured/{id}@get".into(),
            Target::Function("param".into()),
        ),
        (
            "/configured/fixed@get".into(),
            Target::Function("literal".into()),
        ),
        (
            "/configured/{id}/deep@get".into(),
            Target::Function("deep".into()),
        ),
    ])
    .unwrap();
    for (path, method, expected) in [
        ("/configured/fixed", "GET", Some("/configured/fixed")),
        (
            "/configured/synthetic-sensitive",
            "GET",
            Some("/configured/{id}"),
        ),
        ("/configured/id/deep", "GET", Some("/configured/{id}/deep")),
        ("/configured/id/other", "GET", Some("/configured/*")),
        ("/configured/", "GET", Some("/configured/*")),
        ("/other", "GET", Some("/*")),
        ("/", "GET", Some("/*")),
        ("/configured/fixed", "POST", None),
        ("relative", "GET", None),
    ] {
        let mut inv = invocation();
        inv.path = path.into();
        inv.method = method.parse().unwrap();
        assert_eq!(routes.matching_template(&inv.method, &inv.path), expected);
        assert!(inv.route.is_none());
        assert!(inv.path_params.is_empty());
        assert!(inv.query.0.is_empty());
        let mut resolved = inv.clone();
        routes.resolve(&mut resolved);
        assert_eq!(
            resolved.route.as_ref().map(|r| r.template.as_str()),
            expected
        );
    }
}

#[tokio::test]
async fn early_denial_deadline_cancellation_and_warm_mismatch_use_only_trusted_templates() {
    for mode in 0..3 {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let p = setup_sink(records.clone(), &counts, mode == 0);
        for matched in [true, false, true] {
            let mut inv = invocation();
            inv.path = if matched {
                "/configured/synthetic-sensitive-token"
            } else {
                "/synthetic-sensitive-token"
            }
            .into();
            if mode == 1 {
                inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
            }
            if mode == 2 {
                inv.cancel.cancel();
            }
            let response = p.execute(inv, &FakeBackend::new(&counts)).await;
            assert!(!response.status.is_success());
            let data = records.0.lock().unwrap();
            let record = data.last().unwrap();
            assert_eq!(
                record["Route"],
                if matched {
                    "/configured/{id}"
                } else {
                    "<unmatched>"
                }
            );
            assert!(
                !record["Route"]
                    .as_str()
                    .unwrap()
                    .contains("synthetic-sensitive-token")
            );
            assert_eq!(record["Requests"], 1);
            assert_eq!(record["Errors"], 1);
        }
        assert_eq!(n(&counts.backend), 0);
        assert_eq!(n(&counts.response_entries), 3);
        assert_eq!(n(&counts.finalized), 3);
        assert_eq!(records.0.lock().unwrap().len(), 3);
    }
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = setup(&records, &counts);
    let mut inv = invocation();
    inv.path = "/configured/synthetic-sensitive-token".into();
    inv.method = http::Method::POST;
    assert!(
        p.execute(inv, &FakeBackend::new(&counts))
            .await
            .status
            .is_client_error()
    );
    assert_eq!(records.0.lock().unwrap()[0]["Route"], "<unmatched>");
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.response_entries), 1);
    assert_eq!(n(&counts.finalized), 1);
}
#[tokio::test]
async fn matched_backend_template_does_not_emit_caller_path_parameters() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = setup(&records, &counts);
    let mut inv = invocation();
    inv.path = "/configured/synthetic-private-id".into();
    assert_eq!(p.execute(inv, &FakeBackend::new(&counts)).await.status, 200);
    assert_eq!(records.0.lock().unwrap()[0]["Route"], "/configured/{id}");
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.response_entries), 1);
    assert_eq!(n(&counts.finalized), 1);
}
#[tokio::test]
async fn proposed_unmatched_route_contract_prevents_raw_caller_path_disclosure() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = setup(&records, &counts);
    let mut inv = invocation();
    inv.path = "/synthetic-private-path-value".into();
    assert!(
        p.execute(inv, &FakeBackend::new(&counts))
            .await
            .status
            .is_client_error()
    );
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.response_entries), 1);
    assert_eq!(n(&counts.finalized), 1);
    let data = records.0.lock().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["Errors"], 1);
    assert_eq!(
        data[0]["Route"], "<unmatched>",
        "proposed safety amendment: never publish an unmatched caller path as an EMF dimension; owner disposition required for approved missing-target Route expectation"
    );
}

#[test]
fn actual_stdout_emf_uses_trusted_routes_and_closed_shape() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stdout_emf_child", "--nocapture"])
        .env("WP12_EMF_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut records = Vec::new();
    let mut done = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while records.len() != 3 || !done {
        let next =
            receive.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
        let Ok(line) = next else {
            child.kill().unwrap();
            child.wait().unwrap();
            reader.join().unwrap();
            panic!("actual EMF records/producer completion missing");
        };
        if line == "WP12_EMF_PRODUCER_DONE" {
            done = true;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            records.push(value);
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    reader.join().unwrap();
    let end = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for (i, record) in records.iter().enumerate() {
        assert_eq!(
            record
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "Duration",
                "Errors",
                "FunctionName",
                "Requests",
                "Route",
                "_aws"
            ]
        );
        assert_eq!(record["FunctionName"], "test-only-gateway");
        assert_eq!(record["Requests"], 1);
        assert_eq!(record["Errors"], u64::from(i != 0));
        assert_eq!(
            record["Route"],
            if i == 2 {
                "<unmatched>"
            } else {
                "/configured/{id}"
            }
        );
        assert!(!record.to_string().contains("synthetic-sensitive-token"));
        assert!(record["Duration"].as_f64().unwrap() >= 0.0);
        assert!((start..=end).contains(&record["_aws"]["Timestamp"].as_u64().unwrap()));
        let definition = &record["_aws"]["CloudWatchMetrics"][0];
        assert_eq!(definition["Namespace"], "LightLambda");
        assert_eq!(
            definition["Dimensions"],
            serde_json::json!([["FunctionName", "Route"]])
        );
        assert_eq!(
            definition["Metrics"],
            serde_json::json!([{"Name":"Requests","Unit":"Count"},{"Name":"Errors","Unit":"Count"},{"Name":"Duration","Unit":"Milliseconds"}])
        );
    }
}
#[tokio::test]
async fn stdout_emf_child() {
    if std::env::var_os("WP12_EMF_CHILD").is_none() {
        return;
    }
    let counts = Arc::new(Counts::default());
    let p = setup_sink(
        Arc::new(gateway_policy::observation::StdoutSink),
        &counts,
        false,
    );
    for mode in 0..3 {
        let mut inv = invocation();
        inv.path = if mode == 2 {
            "/synthetic-sensitive-token"
        } else {
            "/configured/synthetic-sensitive-token"
        }
        .into();
        if mode == 1 {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        assert_eq!(
            p.execute(inv, &FakeBackend::new(&counts))
                .await
                .status
                .is_success(),
            mode == 0
        );
    }
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.response_entries), 3);
    assert_eq!(n(&counts.finalized), 3);
    println!("WP12_EMF_PRODUCER_DONE");
    loop {
        std::thread::park();
    }
}
