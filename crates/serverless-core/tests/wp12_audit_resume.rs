mod security_support;
use async_trait::async_trait;
use gateway_policy::{
    audit::AuditField,
    observation::{RecordKind, RecordSink},
};
use security_support::*;
use serde_json::{Value, json};
use serverless_core::{handlers::audit::AuditHandler, plan::*, *};
use std::sync::{Arc, Mutex, atomic::Ordering};

#[tokio::test]
async fn actual_audit_stdout_and_full_queue_have_bounded_best_effort_submission() {
    use gateway_policy::observation::StdoutSink;
    use std::{
        sync::{atomic::AtomicBool, mpsc::sync_channel},
        time::Duration,
    };
    const CHILD: &str = "WP12_AUDIT_STDOUT_CHILD";
    const MARKER: &str = "test-only-stdout-audit";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actual_audit_stdout_and_full_queue_have_bounded_best_effort_submission",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "stdout child failed");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let records: Vec<Value> = stdout
            .lines()
            .filter_map(|line| {
                // libtest prints its test-name prefix without a newline.
                let outer: Value = serde_json::from_str(&line[line.find('{')?..]).ok()?;
                let inner: Value = serde_json::from_str(outer["fields"]["audit"].as_str()?).ok()?;
                (inner["serviceId"] == MARKER).then_some(inner)
            })
            .collect();
        assert_eq!(
            records.len(),
            65,
            "first real policy plus 64 queue-admitted probes"
        );
        assert_eq!(records[0]["statusCode"], 200);
        assert_eq!(
            records
                .iter()
                .filter(|v| v.get("queueProbe").is_some())
                .count(),
            64
        );
        assert!(!stdout.contains("test-only-oversize-marker"));
        return;
    }
    let first = Arc::new(AtomicBool::new(true));
    let (started, ready) = sync_channel(1);
    let (release, resume) = sync_channel(1);
    let resume = Arc::new(Mutex::new(resume));
    tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_writer(move || {
            if first.swap(false, Ordering::SeqCst) {
                started.send(()).unwrap();
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            std::io::stdout()
        })
        .try_init()
        .unwrap();
    let audit = Arc::new(
        AuditHandler::from_value(
            &json!({"enabled":true,"auditOnError":false,"statusCode":true,
            "responseTime":false,"headers":[],"audit":["serviceId"]}),
            MARKER.into(),
            Arc::new(StdoutSink),
        )
        .unwrap(),
    );
    let probe = Arc::new(Probe::default());
    let pipeline = pipeline(audit, HandlerId::Audit, probe.clone());
    assert_eq!(
        pipeline
            .execute(invocation(None, "/x"), probe.as_ref())
            .await
            .status,
        200
    );
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(probe.final_principals.lock().unwrap().len(), 1);
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let sink = StdoutSink;
    let kind = RecordKind::Audit {
        error_level: false,
        stack_trace: false,
    };
    for index in 0..65 {
        sink.try_emit(kind, json!({"serviceId":MARKER,"queueProbe":index}));
    }
    sink.try_emit(
        kind,
        json!({"serviceId":"test-only-oversize-marker","large":"x".repeat(65536)}),
    );
    release.send(()).unwrap();
    // Isolated component observation window; no production delivery/time guarantee.
    tokio::time::sleep(Duration::from_secs(1)).await;
}

#[test]
fn disabled_audit_registry_bridge_validates_wrapped_bindings_before_removal() {
    for invalid in 0..4 {
        let mut registry = lifecycle_support::registry();
        let audit = Arc::new(
            AuditHandler::from_value(
                &json!({"enabled":false,"headers":[],"audit":[]}),
                "test-only-service".into(),
                Arc::new(Records::default()),
            )
            .unwrap(),
        );
        assert!(audit.is_disabled_audit());
        let binding = match invalid {
            0 => Binding::Request(audit, RequestCapabilities::default()),
            1 => Binding::Local(audit),
            2 => Binding::Request(
                audit,
                RequestCapabilities {
                    establishes_verified_identity: true,
                    requires_verified_identity: false,
                },
            ),
            _ => Binding::Request(
                audit,
                RequestCapabilities {
                    establishes_verified_identity: false,
                    requires_verified_identity: true,
                },
            ),
        };
        registry.register(HandlerId::Audit, Arc::new(One(binding)));
        let result = ExecutionPlan::compile(
            lifecycle_support::config(
                &[(HandlerId::Audit, "audit"), (HandlerId::Proxy, "proxy")],
                &["audit", "proxy"],
            ),
            json!({}),
            &registry,
        );
        if invalid == 0 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(InitError::FactoryContract(HandlerId::Audit))
            ));
        }
    }
}

#[test]
fn wrapped_enabled_audit_retains_zero_one_and_completion_bound() {
    for count in [0, 1, 64, 65] {
        let mut registry = lifecycle_support::registry();
        let audit = Arc::new(
            AuditHandler::from_value(
                &json!({"headers":[],"audit":[]}),
                "test-only-service".into(),
                Arc::new(Records::default()),
            )
            .unwrap(),
        );
        assert!(!audit.is_disabled_audit());
        registry.register(
            HandlerId::Audit,
            Arc::new(One(Binding::Request(audit, RequestCapabilities::default()))),
        );
        let mut chain = vec!["audit"; count];
        chain.push("proxy");
        let result = ExecutionPlan::compile(
            lifecycle_support::config(
                &[(HandlerId::Audit, "audit"), (HandlerId::Proxy, "proxy")],
                &chain,
            ),
            json!({}),
            &registry,
        );
        if count <= 64 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(InitError::Malformed(
                    "active audit/metrics occurrence limit (64)"
                ))
            ));
        }
    }
}

#[test]
fn audit_removal_does_not_bypass_identity_or_other_handler_defaults() {
    let counts = Arc::new(lifecycle_support::Counts::default());
    let ordinary = Arc::new(lifecycle_support::Handler::new(
        HandlerId::Correlation,
        lifecycle_support::Action::Continue,
        &counts,
    ));
    assert!(!ordinary.is_disabled_audit());
    let mut registry = lifecycle_support::registry();
    registry.register(
        HandlerId::Correlation,
        Arc::new(One(Binding::Request(
            ordinary,
            RequestCapabilities::default(),
        ))),
    );
    let configuration = lifecycle_support::config(
        &[
            (HandlerId::Correlation, "correlation"),
            (HandlerId::Proxy, "proxy"),
        ],
        &["correlation", "proxy"],
    );
    assert!(ExecutionPlan::compile(configuration.clone(), json!({}), &registry).is_ok());
    let disabled = Arc::new(
        AuditHandler::from_value(
            &json!({"enabled":false,"headers":[],"audit":[]}),
            "test-only-service".into(),
            Arc::new(Records::default()),
        )
        .unwrap(),
    );
    registry.register(
        HandlerId::Correlation,
        Arc::new(One(Binding::Request(
            disabled,
            RequestCapabilities::default(),
        ))),
    );
    assert!(matches!(
        ExecutionPlan::compile(configuration, json!({}), &registry),
        Err(InitError::FactoryContract(HandlerId::Correlation))
    ));
    assert_eq!(lifecycle_support::n(&counts.backend), 0);
    assert_eq!(lifecycle_support::n(&counts.response_entries), 0);
    assert_eq!(lifecycle_support::n(&counts.finalized), 0);
}

mod lifecycle_support {
    use super::*;
    use std::{sync::atomic::AtomicUsize, time::Duration};
    #[derive(Default)]
    pub struct Counts {
        pub backend: AtomicUsize,
        pub response_entries: AtomicUsize,
        pub finalized: AtomicUsize,
    }
    pub fn n(value: &AtomicUsize) -> usize {
        value.load(Ordering::SeqCst)
    }
    impl Finalizer for Counts {
        fn finalize(&self, _: &Invocation, state: &ExchangeState, _: &GatewayResponse) {
            assert!(state.metrics.finalized);
            self.finalized.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub enum Action {
        Continue,
        Reject,
        Fail,
    }
    pub struct Handler {
        id: HandlerId,
        action: Action,
        counts: Arc<Counts>,
    }
    impl Handler {
        pub fn new(id: HandlerId, action: Action, counts: &Arc<Counts>) -> Self {
            Self {
                id,
                action,
                counts: counts.clone(),
            }
        }
    }
    #[async_trait]
    impl RequestHandler for Handler {
        fn id(&self) -> HandlerId {
            self.id
        }
        async fn on_request(
            &self,
            _: &mut Invocation,
            _: &mut ExchangeState,
        ) -> Result<HandlerOutcome, HandlerFailure> {
            match self.action {
                Action::Continue => Ok(HandlerOutcome::Continue),
                Action::Reject => Ok(HandlerOutcome::Reject(StatusError::missing_identity())),
                Action::Fail => Err(HandlerFailure),
            }
        }
    }
    #[async_trait]
    impl ResponseHandler for Handler {
        fn id(&self) -> HandlerId {
            self.id
        }
        async fn on_response(
            &self,
            _: &Invocation,
            _: &mut ExchangeState,
            _: &mut GatewayResponse,
        ) -> Result<(), HandlerFailure> {
            self.counts.response_entries.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    pub struct FakeBackend {
        counts: Arc<Counts>,
        pub delay: Duration,
        pub failure: bool,
    }
    impl FakeBackend {
        pub fn new(counts: &Arc<Counts>) -> Self {
            Self {
                counts: counts.clone(),
                delay: Duration::ZERO,
                failure: false,
            }
        }
    }
    #[async_trait]
    impl Backend for FakeBackend {
        async fn dispatch(
            &self,
            inv: &Invocation,
            _: &ExchangeState,
            _: &Target,
        ) -> Result<GatewayResponse, DispatchError> {
            self.counts.backend.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.failure {
                Err(DispatchError(StatusError::new(
                    502,
                    "ERR14019",
                    "BACKEND_UNAVAILABLE",
                    "Synthetic backend unavailable.",
                )))
            } else {
                Ok(GatewayResponse {
                    status: http::StatusCode::OK,
                    headers: Default::default(),
                    body: inv.body.clone(),
                    body_is_binary: false,
                })
            }
        }
    }
    pub fn invocation() -> Invocation {
        security_support::invocation(None, "/x")
    }
    pub fn install(registry: &mut Registry, id: HandlerId, binding: Binding) {
        registry.register(id, Arc::new(One(binding)));
    }
    pub fn registry() -> Registry {
        let mut registry = Registry::default();
        install(
            &mut registry,
            HandlerId::Proxy,
            Binding::Dispatch(
                TargetRoutes::compile([(
                    "/*@get".into(),
                    Target::Function("test-business".into()),
                )])
                .unwrap(),
            ),
        );
        registry
    }
    pub fn config(entries: &[(HandlerId, &str)], selected: &[&str]) -> Value {
        json!({"enabled":true,"handlers":entries.iter().map(|(id,alias)| {
            let class=serverless_core::compat::COMPATIBILITY.iter().find(|c| c.id==*id).unwrap().class;
            format!("{class}@{alias}")
        }).collect::<Vec<_>>(),"defaultHandlers":selected})
    }
    pub fn compile(config: Value, registry: &Registry) -> Arc<ExecutionPlan> {
        Arc::new(ExecutionPlan::compile(config, json!({}), registry).unwrap())
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<Value>>);
impl RecordSink for Records {
    fn try_emit(&self, _: RecordKind, record: Value) {
        self.0.lock().unwrap().push(record);
    }
}
#[tokio::test]
async fn entered_audit_timestamp_literal_cannot_bypass_mandatory_redaction() {
    let records = Arc::new(Records::default());
    let audit = Arc::new(
        AuditHandler::from_value(
            &json!({"enabled":true,"mask":false,"auditOnError":false,
        "statusCode":true,"responseTime":false,"headers":[],"audit":["serviceId"],
        "timestampFormat":"'Bearer test-only-audit-marker'"}),
            "test-only-service".into(),
            records.clone(),
        )
        .unwrap(),
    );
    let probe = Arc::new(Probe::default());
    let pipeline = pipeline(audit, HandlerId::Audit, probe.clone());
    assert_eq!(
        pipeline
            .execute(invocation(None, "/x"), probe.as_ref())
            .await
            .status,
        200
    );
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(probe.final_principals.lock().unwrap().len(), 1);
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["timestamp"], "<redacted>",
        "actual on_request capture must preserve mandatory sink redaction"
    );
}

struct LateAuditField;
#[async_trait]
impl RequestHandler for LateAuditField {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state.audit.fields.set(AuditField::ClientId, "sensitive-id");
        Ok(HandlerOutcome::Continue)
    }
}
#[tokio::test]
async fn late_audit_fields_must_receive_configured_regex_masking_before_emission() {
    let records = Arc::new(Records::default());
    let audit = AuditHandler::from_value(
        &json!({"enabled":true,"mask":true,"auditOnError":false,
        "statusCode":true,"responseTime":false,"headers":[],"audit":["client_id"],
        "timestampFormat":"'synthetic-time'"}),
        "test-only-service".into(),
        records.clone(),
    )
    .unwrap()
    .with_mask_regex(&json!({"audit":{"client_id":"(.+)"}}))
    .unwrap();
    let mut registry = Registry::default();
    registry.register(HandlerId::Audit, Arc::new(audit));
    registry.register(
        HandlerId::Correlation,
        Arc::new(One(Binding::Request(
            Arc::new(LateAuditField),
            Default::default(),
        ))),
    );
    registry.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let plan=ExecutionPlan::compile(json!({"enabled":true,"handlers":[
        format!("{}@audit",class(HandlerId::Audit)),format!("{}@late",class(HandlerId::Correlation)),
        format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["audit","late","proxy"]}),json!({}),&registry).unwrap();
    let probe = Arc::new(Probe::default());
    let pipeline = Pipeline::new(Arc::new(plan), vec![probe.clone()]);
    assert_eq!(
        pipeline
            .execute(invocation(None, "/x"), probe.as_ref())
            .await
            .status,
        200
    );
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(probe.final_principals.lock().unwrap().len(), 1);
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["client_id"], "************",
        "late copies must not bypass supported regex masking"
    );
}

#[test]
fn qualified_java_timestamp_pattern_is_accepted_by_actual_audit_constructor() {
    let pattern = "yyyy-MM-dd HH:mm:ss.SSS";
    assert!(gateway_policy::audit_timestamp::AuditTimestamp::parse(pattern).is_ok());
    let records = Arc::new(Records::default());
    let audit = AuditHandler::from_value(
        &json!({"enabled":true,"headers":[],"audit":["serviceId"],
        "timestampFormat":pattern}),
        "test-only-service".into(),
        records,
    );
    assert!(
        audit.is_ok(),
        "a qualified supported timestamp formatter must be wired into the real handler"
    );
}

#[tokio::test]
async fn formatted_timestamps_and_redacted_fallback_are_invocation_local() {
    for pattern in [
        "yyyy-MM-dd HH:mm:ss.SSS",
        "'é😀'",
        "'Basic synthetic-marker'",
    ] {
        let records = Arc::new(Records::default());
        let audit = Arc::new(
            AuditHandler::from_value(
                &json!({"mask":false,"auditOnError":false,"responseTime":false,
                "headers":[],"audit":["serviceId"],"timestampFormat":pattern}),
                "test-only-service".into(),
                records.clone(),
            )
            .unwrap(),
        );
        let probe = Arc::new(Probe::default());
        let pipeline = pipeline(audit, HandlerId::Audit, probe.clone());
        for early in [false, true] {
            let inv = invocation(None, "/x");
            if early {
                inv.cancel.cancel();
            }
            let response = pipeline.execute(inv, probe.as_ref()).await;
            assert_eq!(response.status.as_u16(), if early { 504 } else { 200 });
        }
        assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(probe.final_principals.lock().unwrap().len(), 2);
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), 2);
        for record in records.iter() {
            let timestamp = record["timestamp"].as_str().unwrap();
            match pattern {
                "'é😀'" => assert_eq!(timestamp, "é😀"),
                "'Basic synthetic-marker'" => assert_eq!(timestamp, "<redacted>"),
                _ => {
                    assert_eq!(timestamp.len(), 23);
                    assert!(
                        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.3f")
                            .is_ok()
                    );
                }
            }
        }
    }
}

#[test]
fn malformed_and_unbounded_timestamp_patterns_reject_without_echo() {
    for pattern in [
        "'synthetic-unclosed".to_owned(),
        "secret-q".to_owned(),
        "x".repeat(1027),
    ] {
        let error = AuditHandler::from_value(
            &json!({"headers":[],"audit":[],"timestampFormat":pattern}),
            "test-only-service".into(),
            Arc::new(Records::default()),
        )
        .err()
        .unwrap();
        assert!(!format!("{error:?}").contains(&pattern));
    }
}

#[test]
fn disabled_actual_audit_handler_prepares_no_completion_descriptor() {
    let handler = AuditHandler::from_value(
        &json!({"enabled":false,"auditOnError":false,"headers":[],"audit":["serviceId"]}),
        "test-only-service".into(),
        Arc::new(Records::default()),
    )
    .unwrap();
    assert!(
        handler
            .prepare_completion(&invocation(None, "/x"))
            .is_none(),
        "disabled Audit must perform no completion preparation"
    );
}

#[tokio::test]
async fn disabled_actual_audit_binding_emits_nothing_on_success_and_early_exit() {
    let records = Arc::new(Records::default());
    let audit = Arc::new(
        AuditHandler::from_value(
            &json!({"enabled":false,"auditOnError":false,"headers":[],"audit":["serviceId"]}),
            "test-only-service".into(),
            records.clone(),
        )
        .unwrap(),
    );
    let probe = Arc::new(Probe::default());
    let pipeline = pipeline(audit, HandlerId::Audit, probe.clone());
    for mode in 0..3 {
        let mut inv = invocation(None, "/x");
        if mode == 1 {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        if mode == 2 {
            inv.cancel.cancel();
        }
        let response = pipeline.execute(inv, probe.as_ref()).await;
        assert_eq!(response.status.as_u16(), if mode == 0 { 200 } else { 504 });
    }
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(probe.final_principals.lock().unwrap().len(), 3);
    assert!(
        records.0.lock().unwrap().is_empty(),
        "disabled Audit must be removed from selected policies, including early-exit preparation"
    );
}

#[test]
fn disabled_actual_audit_factory_does_not_consume_the_active_completion_bound() {
    let records = Arc::new(Records::default());
    let mut registry = Registry::default();
    registry.register(
        HandlerId::Audit,
        Arc::new(
            AuditHandler::from_value(
                &json!({"enabled":false,"headers":[],"audit":[]}),
                "test-only-service".into(),
                records.clone(),
            )
            .unwrap(),
        ),
    );
    registry.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let mut selected = vec!["audit"; 65];
    selected.push("proxy");
    let result = ExecutionPlan::compile(
        json!({"enabled":true,"handlers":[
        format!("{}@audit",class(HandlerId::Audit)),format!("{}@proxy",class(HandlerId::Proxy))],
        "defaultHandlers":selected}),
        json!({}),
        &registry,
    );
    assert!(records.0.lock().unwrap().is_empty());
    assert!(
        result.is_ok(),
        "disabled actual Audit occurrences must be removed before the active bound"
    );
}

#[test]
fn nested_disabled_audit_occurrences_are_excluded_without_weakening_enabled_bound() {
    for enabled in [false, true] {
        let mut registry = lifecycle_support::registry();
        registry.register(
            HandlerId::Audit,
            Arc::new(
                AuditHandler::from_value(
                    &json!({"enabled":enabled,"headers":[],"audit":[]}),
                    "test-only-service".into(),
                    Arc::new(Records::default()),
                )
                .unwrap(),
            ),
        );
        let mut config = lifecycle_support::config(
            &[(HandlerId::Audit, "audit"), (HandlerId::Proxy, "proxy")],
            &["outer", "proxy"],
        );
        config["chains"] = json!({"inner":["audit"],"outer":vec!["inner";65]});
        let result = ExecutionPlan::compile(config, json!({}), &registry);
        if enabled {
            assert!(matches!(
                result,
                Err(InitError::Malformed(
                    "active audit/metrics occurrence limit (64)"
                ))
            ));
        } else {
            assert!(
                result.is_ok(),
                "nested disabled Audit must be removed before active accounting"
            );
        }
    }
}

#[tokio::test]
async fn disabled_direct_audit_preserves_invocation_state_and_strict_validation() {
    let records = Arc::new(Records::default());
    let handler = AuditHandler::from_value(
        &json!({"enabled":false,"headers":[],"audit":["client_id"]}),
        "test-only-service".into(),
        records.clone(),
    )
    .unwrap();
    let mut inv = invocation(None, "/x");
    inv.headers
        .insert("x-preserved", "synthetic".parse().unwrap());
    let mut state = ExchangeState::default();
    state.principal = Some(Principal::from_verified(
        "test-only".into(),
        Some("synthetic-client".into()),
        None,
        json!({}),
    ));
    state
        .audit
        .fields
        .set(AuditField::ClientId, "synthetic-client");
    state
        .response_headers
        .insert("x-preserved", "synthetic".parse().unwrap());
    assert!(matches!(
        handler.on_request(&mut inv, &mut state).await.unwrap(),
        HandlerOutcome::Continue
    ));
    assert_eq!(inv.headers["x-preserved"], "synthetic");
    assert_eq!(state.response_headers["x-preserved"], "synthetic");
    assert_eq!(
        state.principal.as_ref().unwrap().client_id(),
        Some("synthetic-client")
    );
    assert_eq!(
        state.audit.fields.get("client_id"),
        Some(&json!("synthetic-client"))
    );
    assert!(records.0.lock().unwrap().is_empty());
    for enabled in [Value::Null, json!("false"), json!(0), json!([])] {
        assert!(
            AuditHandler::from_value(
                &json!({"enabled":enabled}),
                "test-only-service".into(),
                records.clone()
            )
            .is_err()
        );
    }
    for invalid in [
        json!({"enabled":false,"timestampFormat":"'unclosed"}),
        json!({"enabled":false,"audit":["responseBody"]}),
        json!({"enabled":false,"mask":"false"}),
    ] {
        assert!(
            AuditHandler::from_value(&invalid, "test-only-service".into(), records.clone())
                .is_err()
        );
    }
}

fn disabled_lifecycle_pipeline(
    records: &Arc<Records>,
    counts: &Arc<lifecycle_support::Counts>,
    mode: u8,
) -> Pipeline {
    use lifecycle_support::{Action, Handler};
    let mut registry = lifecycle_support::registry();
    registry.register(
        HandlerId::Audit,
        Arc::new(
            AuditHandler::from_value(
                &json!({"enabled":false,"auditOnError":false,"headers":[],"audit":["serviceId"]}),
                "test-only-service".into(),
                records.clone(),
            )
            .unwrap(),
        ),
    );
    lifecycle_support::install(
        &mut registry,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    let mut entries = vec![
        (HandlerId::Audit, "audit"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let mut selected = vec!["audit", "proxy", "response"];
    if mode >= 6 {
        entries.insert(0, (HandlerId::RequestHeader, "before"));
        selected.insert(0, "before");
        lifecycle_support::install(
            &mut registry,
            HandlerId::RequestHeader,
            Binding::Request(
                Arc::new(Handler::new(
                    HandlerId::RequestHeader,
                    if mode == 6 {
                        Action::Reject
                    } else {
                        Action::Fail
                    },
                    counts,
                )),
                Default::default(),
            ),
        );
    }
    Pipeline::new(
        lifecycle_support::compile(lifecycle_support::config(&entries, &selected), &registry),
        vec![counts.clone()],
    )
}

#[tokio::test]
async fn disabled_audit_error_timeout_denial_and_preparation_paths_emit_nothing_and_finalize_once()
{
    for mode in 0..8 {
        let records = Arc::new(Records::default());
        let counts = Arc::new(lifecycle_support::Counts::default());
        let pipeline = disabled_lifecycle_pipeline(&records, &counts, mode);
        let mut inv = lifecycle_support::invocation();
        let mut backend = lifecycle_support::FakeBackend::new(&counts);
        if mode == 1 {
            backend.failure = true;
        }
        if mode == 2 {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        if mode == 3 {
            inv.cancel.cancel();
        }
        if mode == 4 {
            inv.deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
            backend.delay = std::time::Duration::from_millis(100);
        }
        let response = pipeline
            .execute_prepared(inv, &backend, |response| {
                if mode == 5 {
                    Err(StatusError::handler_failure())
                } else {
                    Ok(response)
                }
            })
            .await;
        assert_eq!(
            response.status.as_u16(),
            [200, 502, 504, 504, 504, 500, 401, 500][mode as usize]
        );
        assert_eq!(
            lifecycle_support::n(&counts.backend),
            usize::from([0, 1, 4, 5].contains(&mode))
        );
        assert_eq!(lifecycle_support::n(&counts.response_entries), 1);
        assert_eq!(lifecycle_support::n(&counts.finalized), 1);
        assert!(records.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn disabled_audit_caller_drop_drains_framework_once_without_a_record() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(lifecycle_support::Counts::default());
    let pipeline = disabled_lifecycle_pipeline(&records, &counts, 0);
    let mut backend = lifecycle_support::FakeBackend::new(&counts);
    backend.delay = std::time::Duration::from_secs(10);
    let caller = tokio::spawn(async move {
        pipeline
            .execute(lifecycle_support::invocation(), &backend)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while lifecycle_support::n(&counts.backend) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(lifecycle_support::n(&counts.backend), 1);
    assert_eq!(lifecycle_support::n(&counts.response_entries), 0);
    assert_eq!(lifecycle_support::n(&counts.finalized), 1);
    assert!(records.0.lock().unwrap().is_empty());
}
