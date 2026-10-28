//! Partial X-04 qualification through real factories and native reservations.
use super::support::*;
use serde_json::{Value, json};
use serverless_core::HandlerId;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::SeqCst},
};

const ROUTE: &str = "/qualification/logger";
fn class(id: HandlerId) -> &'static str {
    serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == id)
        .unwrap()
        .class
}
fn profile(dir: &std::path::Path, enabled: bool, auth: bool, route: &str) {
    update_values(dir, "logger.enabled", json!(enabled));
    if enabled && auth && route == ROUTE {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/logger-set-qualification");
        for name in ["handler.yml", "logging.yml"] {
            std::fs::copy(source.join(name), dir.join(name)).unwrap();
        }
        return;
    }
    let mut handlers = vec![
        format!("{}@setLogger", class(HandlerId::SetLogger)),
        format!("{}@getLogger", class(HandlerId::GetLogger)),
        format!("{}@proxy", class(HandlerId::Proxy)),
    ];
    if auth {
        handlers.push(format!("{}@jwt", class(HandlerId::Jwt)));
    }
    let chain = |terminal| {
        if auth {
            vec!["jwt", terminal]
        } else {
            vec![terminal]
        }
    };
    std::fs::write(
        dir.join("handler.yml"),
        json!({
            "handlers": handlers,
            "paths": [{"path":route,"method":"post","exec":chain("setLogger")},
                      {"path":route,"method":"get","exec":chain("getLogger")}],
            "defaultHandlers":["proxy"]
        })
        .to_string(),
    )
    .unwrap();
}
fn request(route: &str, method: &str, token: Option<&str>, body: Option<&str>) -> Value {
    let mut value = event(route, method, token);
    value["body"] = json!(body);
    value["multiValueHeaders"]["content-type"] = json!(["application/json"]);
    value
}
fn directive(level: &str) -> String {
    json!([{"name":"ROOT","level":level}]).to_string()
}

#[tokio::test]
async fn gated_factory_admission_and_reserved_native_route_remain_explicit() {
    for (name, gate, enabled, auth, filter, expected) in [
        ("absent", None, true, true, "info", "logger SET unsupported"),
        (
            "zero",
            Some("0"),
            true,
            true,
            "info",
            "logger SET unsupported",
        ),
        (
            "invalid-gate",
            Some("true"),
            true,
            true,
            "info",
            "invalid trusted startup gate",
        ),
        (
            "disabled",
            Some("1"),
            false,
            true,
            "info",
            "logger SET unsupported",
        ),
        (
            "compound",
            Some("1"),
            true,
            true,
            "info,light_security=trace",
            "scalar INFO/DEBUG/TRACE",
        ),
        (
            "suppressing",
            Some("1"),
            true,
            true,
            "warn",
            "scalar INFO/DEBUG/TRACE",
        ),
        (
            "invalid-filter",
            Some("1"),
            true,
            true,
            "[",
            "scalar INFO/DEBUG/TRACE",
        ),
        (
            "no-jwt",
            Some("1"),
            true,
            false,
            "info",
            "verified JWT runtime",
        ),
    ] {
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
        let dir = config(&invoke, &jwks);
        profile(dir.path(), enabled, auth, ROUTE);
        let runtime = Runtime::start(vec![], false, false, 202).await;
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.env("RUST_LOG", filter);
        if let Some(gate) = gate {
            cmd.env("LIGHT_LAMBDA_ALLOW_LOGGER_SET", gate);
        }
        let result = Process::spawn(cmd).collect(false).await;
        result.save(
            &format!("logger-set-admission-{name}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
        assert_eq!(result.status.code(), Some(1), "{name}");
        assert!(!result.stopped_by_harness);
        let records = runtime.records();
        assert_eq!(records.len(), 1, "{name}");
        assert!(records[0].path.ends_with("/init/error"));
        assert!(
            records[0].json()["errorMessage"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{name}: {:?}",
            records[0].json()
        );
        assert_eq!(runtime.state.polls.load(SeqCst), 0);
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
    }
    // Actual reserved operation must resolve to the opted-in local factory.
    let keys = Keys::new("logger-set-reserved");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    profile(dir.path(), true, true, "/adm/logger");
    let token = keys.token(claims("logger-admin"));
    let runtime = Runtime::start(
        vec![
            Invocation::new(
                "reserved",
                request(
                    "/adm/logger",
                    "POST",
                    Some(&token),
                    Some(&directive("DEBUG")),
                ),
            ),
            Invocation::new(
                "unchanged",
                request("/adm/logger", "GET", Some(&token), None),
            ),
        ],
        false,
        false,
        202,
    )
    .await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.env("LIGHT_LAMBDA_ALLOW_LOGGER_SET", "1")
        .env("RUST_LOG", "info");
    let process = Process::spawn(cmd);
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    result.save(
        "logger-set-native-reservation-open",
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_eq!(runtime.records()[0].json()["statusCode"], 200);
    assert_eq!(runtime.records()[1].json()["body"], r#"{"filter":"debug"}"#);
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 1);
}

#[tokio::test]
async fn real_factory_built_app_commands_and_warm_authorization_are_atomic() {
    let keys = Keys::new("logger-set-warm");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    profile(dir.path(), true, true, ROUTE);
    let token = keys.token(claims("logger-admin"));
    let mut events = vec![];
    let mut expected = vec![];
    let mut push = |name: &str,
                    method: &str,
                    token: Option<&str>,
                    body: Option<&str>,
                    status: u16,
                    filter: Option<&str>| {
        events.push(Invocation::new(name, request(ROUTE, method, token, body)));
        expected.push((status, filter.map(str::to_owned)));
    };
    push("initial", "GET", Some(&token), None, 200, Some("info"));
    push(
        "unauthorized",
        "POST",
        None,
        Some(&directive("TRACE")),
        401,
        None,
    );
    push(
        "forged",
        "POST",
        Some("not-a-token"),
        Some(&directive("TRACE")),
        401,
        None,
    );
    push(
        "debug",
        "POST",
        Some(&token),
        Some(&directive("DEBUG")),
        200,
        None,
    );
    push(
        "actual-debug",
        "GET",
        Some(&token),
        None,
        200,
        Some("debug"),
    );
    push("missing", "POST", Some(&token), None, 400, None);
    push("empty", "POST", Some(&token), Some(""), 400, None);
    for (i, body) in [
        "null",
        "{}",
        "[",
        "[{}]",
        "[{\"name\":\"other\",\"level\":\"INFO\"}]",
        "[{\"name\":\"ROOT\",\"level\":\"INFO\",\"extra\":0}]",
        "[{\"name\":\"ROOT\",\"level\":\"INFO\"},{\"name\":\"ROOT\",\"level\":\"TRACE\"}]",
    ]
    .iter()
    .enumerate()
    {
        push(
            &format!("bad-{i}"),
            "POST",
            Some(&token),
            Some(body),
            400,
            None,
        );
        push(
            &format!("unchanged-{i}"),
            "GET",
            Some(&token),
            None,
            200,
            Some("debug"),
        );
    }
    for level in ["OFF", "WARN", "ERROR", "ALL", "info"] {
        push(
            &format!("level-{level}"),
            "POST",
            Some(&token),
            Some(&directive(level)),
            400,
            None,
        );
    }
    push("noop", "POST", Some(&token), Some("[]"), 200, None);
    push("after-noop", "GET", Some(&token), None, 200, Some("debug"));
    let mut boundary = directive("TRACE");
    boundary.extend(std::iter::repeat_n(' ', 1024 - boundary.len()));
    push("boundary", "POST", Some(&token), Some(&boundary), 200, None);
    push(
        "actual-trace",
        "GET",
        Some(&token),
        None,
        200,
        Some("trace"),
    );
    boundary.push(' ');
    push(
        "over-boundary",
        "POST",
        Some(&token),
        Some(&boundary),
        400,
        None,
    );
    push("after-over", "GET", Some(&token), None, 200, Some("trace"));
    push(
        "warm-no-auth",
        "POST",
        None,
        Some(&directive("INFO")),
        401,
        None,
    );
    push(
        "after-no-auth",
        "GET",
        Some(&token),
        None,
        200,
        Some("trace"),
    );
    push(
        "restore",
        "POST",
        Some(&token),
        Some(&directive("INFO")),
        200,
        None,
    );
    push("final", "GET", Some(&token), None, 200, Some("info"));
    let count = events.len();
    let runtime = Runtime::start(events, false, false, 202).await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.env("LIGHT_LAMBDA_ALLOW_LOGGER_SET", "1")
        .env("RUST_LOG", "info");
    let process = Process::spawn(cmd);
    runtime.wait_posts(count).await;
    let result = process.collect(true).await;
    result.save("logger-set-real-built-app", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, count);
    assert_audits(&result, count);
    for (post, (status, filter)) in runtime.records().iter().zip(expected) {
        let response = post.json();
        assert_eq!(response["statusCode"], status, "{:?}", response);
        let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
        if let Some(filter) = filter {
            assert_eq!(body, json!({"filter":filter}));
        }
        if status == 400 {
            assert!(matches!(
                body["code"].as_str(),
                Some("ERR10059" | "ERR11004")
            ));
        }
    }
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 1);
    assert!(!result.stdout.contains(&token));
}

struct NoDispatch(AtomicUsize);
#[async_trait::async_trait]
impl serverless_core::Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &serverless_core::Invocation,
        _: &serverless_core::ExchangeState,
        _: &serverless_core::Target,
    ) -> std::result::Result<serverless_core::GatewayResponse, serverless_core::DispatchError> {
        self.0.fetch_add(1, SeqCst);
        panic!("local setter must never dispatch");
    }
}
async fn service(
    snapshot: Arc<light_lambda_runtime::init::Snapshot>,
    token: &str,
    id: &str,
    method: &str,
    body: Option<&str>,
) -> Value {
    let mut context = lambda_runtime::Context::default();
    context.request_id = id.into();
    context.deadline = now_ms() + 10_000;
    light_lambda_runtime::service::handle(
        snapshot,
        lambda_runtime::LambdaEvent::new(request(ROUTE, method, Some(token), body), context),
    )
    .await
    .unwrap()
}

// Runs only when explicitly invoked by the parent below, in a fresh test process.
#[tokio::test]
#[ignore = "subprocess-only application probe, launched by the qualification test"]
async fn subscriber_child() {
    let Ok(mode) = std::env::var("LOGGER_SET_TEST_CHILD") else {
        return;
    };
    light_lambda_runtime::telemetry::initialize().unwrap();
    let keys = Keys::new("logger-set-parallel");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    profile(dir.path(), true, true, ROUTE);
    let backend = Arc::new(NoDispatch(AtomicUsize::new(0)));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = light_lambda_runtime::init::BackendConfiguration::Injected(backend.clone());
    let report = light_lambda_runtime::init::validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert_eq!(jwks.count(), 0);
    let snapshot = light_lambda_runtime::init::initialize(input).await.unwrap();
    let token = keys.token(claims("parallel-admin"));
    let initial = service(snapshot.clone(), &token, "initial", "GET", None).await;
    assert_eq!(initial["body"], r#"{"filter":"info"}"#);
    if mode == "reset" {
        println!("LOGGER_RESET_ACTUAL_INFO");
        return;
    }
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..32 {
        let snapshot = snapshot.clone();
        let token = token.clone();
        tasks.spawn(async move {
            let valid = i % 2 == 0;
            let body = directive(if valid {
                if i % 4 == 0 { "TRACE" } else { "DEBUG" }
            } else {
                "OFF"
            });
            let response = service(
                snapshot,
                &token,
                &format!("parallel-{i}"),
                "POST",
                Some(&body),
            )
            .await;
            assert_eq!(response["statusCode"], if valid { 200 } else { 400 });
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    assert_eq!(jwks.count(), 1);
    assert_eq!(backend.0.load(SeqCst), 0);
    for (level, expected) in [("DEBUG", "debug"), ("INFO", "info"), ("TRACE", "trace")] {
        assert_eq!(
            service(
                snapshot.clone(),
                &token,
                level,
                "POST",
                Some(&directive(level))
            )
            .await["statusCode"],
            200
        );
        assert_eq!(
            service(
                snapshot.clone(),
                &token,
                &format!("read-{level}"),
                "GET",
                None
            )
            .await["body"],
            json!({"filter":expected}).to_string()
        );
    }
    assert!(light_lambda_runtime::telemetry::initialize().is_err());
    assert_eq!(
        light_lambda_runtime::telemetry::installed_filter(),
        Some("trace")
    );
    assert_eq!(
        service(
            snapshot,
            &token,
            "end-trace",
            "POST",
            Some(&directive("TRACE"))
        )
        .await["statusCode"],
        200
    );
    assert_eq!(
        light_lambda_runtime::telemetry::installed_filter(),
        Some("trace")
    );
    println!("LOGGER_PROCESS_END_TRACE");
    println!(
        "LOGGER_CONCURRENT_SERVICE_PROOF requests=40 validParallel=16 invalidParallel=16 invokes=0 jwks=1"
    );
}

#[test]
fn actual_subscriber_concurrency_and_fresh_process_reset() {
    for mode in ["parallel", "reset"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "--ignored",
                "logger_set::subscriber_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("LOGGER_SET_TEST_CHILD", mode)
            .env("LIGHT_LAMBDA_ALLOW_LOGGER_SET", "1")
            .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
            .env("RUST_LOG", "info")
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if let Some(root) = std::env::var_os("WP10_L1_EVIDENCE_DIR") {
            let root = std::path::PathBuf::from(root).join(format!("logger-set-subscriber-{mode}"));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("stdout.log"), &text).unwrap();
            std::fs::write(root.join("stderr.log"), &stderr).unwrap();
        }
        assert!(
            output.status.success(),
            "subscriber {mode} failed: {text} {stderr}"
        );
        assert!(
            !text.contains("logger-private-sentinel")
                && !stderr.contains("logger-private-sentinel")
        );
        assert!(
            !text.contains("logger-panic-private-sentinel")
                && !stderr.contains("logger-panic-private-sentinel")
        );
        if mode == "parallel" {
            assert!(text.contains("LOGGER_PROCESS_END_TRACE"));
            assert!(text.contains("LOGGER_CONCURRENT_SERVICE_PROOF"));
            let audit = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| v["fields"]["finalizations"] == 1)
                .count();
            assert_eq!(audit, 40);
        } else {
            assert!(text.contains("LOGGER_RESET_ACTUAL_INFO"));
        }
    }
}

#[tokio::test]
async fn native_reservation_rejections_preserve_actual_filter_and_never_dispatch() {
    for mode in [
        "missing", "invalid", "skipped", "order", "no-chain", "terminal", "method", "default",
        "proxy",
    ] {
        let keys = Keys::new("native-reservation-negative");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        profile(dir.path(), true, true, "/adm/logger");
        let path = dir.path().join("handler.yml");
        let mut cfg: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Independent local getter witnesses the actual subscriber after rejection.
        cfg["paths"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path":ROUTE,"method":"get","exec":["jwt","getLogger"]}));
        match mode {
            "order" => cfg["paths"][0]["exec"] = json!(["setLogger", "jwt"]),
            "no-chain" => cfg["paths"][0]["exec"] = json!(["setLogger"]),
            "terminal" => cfg["paths"][0]["exec"] = json!(["jwt", "getLogger"]),
            "proxy" => cfg["paths"][0]["exec"] = json!(["jwt", "proxy"]),
            "default" => {
                cfg["paths"].as_array_mut().unwrap().remove(0);
            }
            "skipped" => update_values(
                dir.path(),
                "security.skipPathPrefixes",
                json!(["/adm/logger"]),
            ),
            _ => {}
        }
        std::fs::write(path, cfg.to_string()).unwrap();
        let token = keys.token(claims("native-admin"));
        let auth = match mode {
            "missing" => None,
            "invalid" => Some("invalid-token"),
            _ => Some(token.as_str()),
        };
        let method = if mode == "method" { "DELETE" } else { "POST" };
        let runtime = Runtime::start(
            vec![
                Invocation::new("before", request(ROUTE, "GET", Some(&token), None)),
                Invocation::new(
                    "reject",
                    request("/adm/logger", method, auth, Some(&directive("TRACE"))),
                ),
                Invocation::new("after", request(ROUTE, "GET", Some(&token), None)),
            ],
            false,
            false,
            202,
        )
        .await;
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.env("LIGHT_LAMBDA_ALLOW_LOGGER_SET", "1")
            .env("RUST_LOG", "info");
        let process = Process::spawn(cmd);
        for _ in 0..200 {
            let records = runtime.records();
            if records.len() >= 3 || records.iter().any(|r| r.path.ends_with("/init/error")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let admission = runtime
            .records()
            .iter()
            .any(|r| r.path.ends_with("/init/error"));
        let result = process.collect(!admission).await;
        result.save(
            &format!("logger-set-native-reject-{mode}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
        if admission {
            assert_eq!(
                mode,
                "order",
                "unexpected Init rejection {mode}: {:?}",
                runtime.records()[0].json()
            );
            assert_eq!(result.status.code(), Some(1));
            assert_eq!(runtime.state.polls.load(SeqCst), 0);
            assert_eq!(invoke.count(), 0);
            assert_eq!(jwks.count(), 0);
            continue;
        }
        assert_posts(&runtime, 3);
        assert_audits(&result, 3);
        let records = runtime.records();
        for index in [0, 2] {
            assert_eq!(records[index].json()["statusCode"], 200, "{mode}");
            assert_eq!(
                records[index].json()["body"],
                r#"{"filter":"info"}"#,
                "{mode}"
            );
        }
        let expected = if matches!(mode, "missing" | "invalid" | "skipped") {
            401
        } else {
            404
        };
        assert_eq!(
            records[1].json()["statusCode"],
            expected,
            "{mode}: {:?}",
            records[1].json()
        );
        assert_eq!(invoke.count(), 0, "{mode}");
        assert_eq!(jwks.count(), 1, "{mode}");
    }
}
