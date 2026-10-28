//! Test harnesses only. The application main and precision setup are never called.
#[allow(dead_code)]
#[path = "../src/bin/validator_lambda_qualification.rs"]
mod entry;

#[test]
fn compiled_pins_and_configuration_are_the_reviewed_required_profile() {
    let handler: serde_json::Value=serde_json::from_str(r#"{"defaultHandlers":["spec","validator","proxy"],"handlers":["com.networknt.aws.lambda.handler.middleware.specification.OpenApiMiddleware@spec","com.networknt.aws.lambda.handler.middleware.validator.ValidatorMiddleware@validator","com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy"]}"#).unwrap();
    assert_eq!(
        light_lambda_runtime::required_validator_profile::handler_identity_for_review(&handler)
            .unwrap(),
        entry::required_profile().handler_sha256
    );
    assert_eq!(entry::required_profile().active_handlers.len(), 5);
    let pin = entry::catalogue_pin();
    assert_eq!(
        pin.source_bytes,
        include_bytes!("../migrations/native-petstore/openapi.yaml").len()
    );
    assert_eq!(pin.settings.skip_body_validation, Some(false));
    assert_eq!(pin.settings.handle_nullable_field, Some(true));
    assert_eq!(pin.settings.skip_path_prefixes, Some(vec![]));
    assert_eq!(entry::CONFIG, "/opt/config");
    assert_eq!(entry::WORKER, "/opt/precision/precision-worker");
}

#[test]
fn entry_is_separate_and_has_no_stdin_or_success_forcing_exit() {
    let text = include_str!("../src/bin/validator_lambda_qualification.rs");
    assert!(!text.contains("std::io::stdin"));
    assert!(!text.contains("process::exit("));
}

/// Deliberately invoked by the owned loopback reporter harness. Not a parent or
/// worker subprocess: failing preparation cannot construct either engine/Snapshot.
#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit non-child extension/Runtime API fixture"]
async fn registered_failure_fixture() {
    use light_lambda_runtime::{
        init_error::InitError,
        owned_runtime::{CleanupFailure, ExitKind, Primary},
        qualification_shutdown::run_qualification,
        validator_ipc::Failure,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    let mode = std::env::var("OWNED_REPORT_CASE").unwrap();
    assert!(
        [
            "init-error",
            "prepare-panic",
            "cleanup-error",
            "cleanup-panic",
            "cleanup-timeout",
            "cleanup-overrun",
            "registration-fail",
            "rejected-report",
            "held-cleanup"
        ]
        .contains(&mode.as_str())
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let marker = std::env::var("OWNED_REPORT_MARKER").unwrap();
    let authority = std::env::var("AWS_LAMBDA_RUNTIME_API").unwrap();
    let panic_input = mode == "prepare-panic";
    let cleanup_error = mode == "cleanup-error";
    let held = mode == "held-cleanup";
    let cleanup_panic = mode == "cleanup-panic";
    let cleanup_timeout = mode == "cleanup-timeout";
    let cleanup_overrun = mode == "cleanup-overrun";
    let registration_fail = mode == "registration-fail";
    let prepared = Arc::new(AtomicUsize::new(0));
    let prepare_count = prepared.clone();
    let span = std::rc::Rc::new(std::cell::RefCell::new(None));
    let held_span = span.clone();
    let outcome = run_qualification(
        Some(&authority),
        Ok(std::future::pending()),
        move || {
            prepare_count.fetch_add(1, SeqCst);
            *held_span.borrow_mut() = Some(tracing_span());
            assert!(!panic_input, "synthetic preparation panic payload");
            Err(InitError::UnknownHandler(
                "synthetic/private-class-marker".into(),
            ))
        },
        None,
        move |kind, _deadline| async move {
            assert_eq!(kind, ExitKind::InitFailure);
            assert_eq!(counted.fetch_add(1, SeqCst), 0);
            assert!(!cleanup_panic, "synthetic cleanup panic payload");
            if cleanup_timeout {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            if cleanup_overrun {
                // Deliberately ready-after-deadline: Tokio timeout alone polls
                // this synchronous completion before observing its timer.
                std::thread::sleep(std::time::Duration::from_millis(380));
            }
            if held {
                let path = std::path::Path::new(&marker);
                std::fs::write(path.with_extension("entered"), b"entered").unwrap();
                while !path.with_extension("release").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            std::fs::write(marker, b"mock-cleanup-completed").unwrap();
            if cleanup_error {
                Err(Failure::NoReady)
            } else {
                Ok(())
            }
        },
    )
    .await;
    drop(span.borrow_mut().take());
    assert_eq!(calls.load(SeqCst), 1);
    assert_eq!(prepared.load(SeqCst), usize::from(!registration_fail));
    assert!(!outcome.completed_without_failure());
    assert!(matches!(outcome.primary, Primary::Init(_)));
    if cleanup_panic {
        assert_eq!(outcome.cleanup, Err(CleanupFailure::Panic));
    } else if cleanup_timeout || cleanup_overrun {
        assert_eq!(
            outcome.cleanup,
            Err(CleanupFailure::Engine(Failure::Deadline))
        );
    } else if registration_fail || cleanup_error {
        assert_eq!(
            outcome.cleanup,
            Err(CleanupFailure::Engine(Failure::NoReady))
        );
    } else {
        assert!(outcome.cleanup.is_ok());
    }
    println!("OWNED_REPORT_FIXTURE_PASSED");
}

fn tracing_span() -> lambda_runtime::tracing::span::EnteredSpan {
    lambda_runtime::tracing::info_span!(
        "private_test_span",
        token = "SYNTHETIC_SURROUNDING_SPAN_SECRET",
        payload = "synthetic/private-class-marker"
    )
    .entered()
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_external_plan_refuses_before_real_preparation_and_drains_once() {
    use light_lambda_runtime::{
        owned_runtime::{ExitKind, Primary},
        qualification_extension_bridge::run_external,
        validator_ipc::supervision::Policy,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    let prepared = Arc::new(AtomicUsize::new(0));
    let cleaned = Arc::new(AtomicUsize::new(0));
    let p = prepared.clone();
    let c = cleaned.clone();
    let policy = Policy {
        parent_requested_bytes: 1,
        worker_requested_bytes: 512 * 1024 * 1024,
        headroom_bytes: 256 * 1024 * 1024,
        platform_bytes: 1024 * 1024 * 1024,
        worker_address_space_bytes: 1024 * 1024 * 1024,
        lifetime_cpu_seconds: 16,
    };
    let outcome = run_external(
        policy,
        Ok(std::future::pending()),
        move || {
            p.fetch_add(1, SeqCst);
            Err(light_lambda_runtime::init_error::InitError::Preparation)
        },
        None,
        move |kind, _| async move {
            assert_eq!(kind, ExitKind::InitFailure);
            c.fetch_add(1, SeqCst);
            Ok(())
        },
    )
    .await;
    assert_eq!(prepared.load(SeqCst), 0);
    assert_eq!(cleaned.load(SeqCst), 1);
    assert!(matches!(outcome.primary, Primary::Init(_)));
    assert!(!outcome.completed_without_failure());
}
#[test]
fn separate_external_target_never_launches_a_worker_or_stdin_controller() {
    let source = include_str!("../src/bin/validator_shutdown_extension.rs");
    assert!(!source.contains("std::process::Command"));
    assert!(!source.contains("stdin"));
    assert!(!source.contains("process::exit("));
    assert!(include_str!("../src/bin/validator_lambda_qualification.rs").contains("run_external("));
}

mod accounted_profile_composition {
    use super::entry;
    use async_trait::async_trait;
    use light_lambda_runtime::{
        init::{self, BackendConfiguration},
        init_error::InitError,
        validator_ipc::{
            Engine, Failure, Session, Settings,
            accounted_profile::Limits,
            lifecycle::Lifecycle,
            supervision::{Authority, Phase},
        },
    };
    use serde_json::json;
    use serverless_core::*;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
        time::Duration,
    };
    use tokio::time::Instant;

    // These tests share the deliberately process-wide trusted preparation gate.
    // Serialize only this module's Init cases; never the complete application suite.
    static INIT_CASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    struct BackendCount(AtomicUsize);
    #[async_trait]
    impl Backend for BackendCount {
        async fn dispatch(
            &self,
            _: &Invocation,
            _: &ExchangeState,
            _: &Target,
        ) -> Result<GatewayResponse, DispatchError> {
            self.0.fetch_add(1, SeqCst);
            panic!("rejected/offline profile must never dispatch")
        }
    }
    struct Counted {
        engine: Arc<dyn Engine>,
        calls: AtomicUsize,
        source_refusals: AtomicUsize,
    }
    #[async_trait]
    impl Engine for Counted {
        fn configuration_preparation(
            &self,
        ) -> Option<Arc<light_lambda_runtime::init::PreparationGate>> {
            self.engine.configuration_preparation()
        }
        async fn initialize(
            &self,
            source: Arc<[u8]>,
            format: gateway_policy::openapi_operation::SourceFormat,
            settings: Settings,
            deadline: Instant,
        ) -> Result<Arc<dyn Session>, Failure> {
            self.initialize_owned(source, format, Arc::new(settings), deadline, None)
                .await
        }
        async fn initialize_owned(
            &self,
            source: Arc<[u8]>,
            format: gateway_policy::openapi_operation::SourceFormat,
            settings: Arc<Settings>,
            deadline: Instant,
            context: Option<Arc<init::PreparationContext>>,
        ) -> Result<Arc<dyn Session>, Failure> {
            self.calls.fetch_add(1, SeqCst);
            let result = self
                .engine
                .initialize_owned(source, format, settings, deadline, context)
                .await;
            if matches!(&result, Err(Failure::ParentBudget)) {
                self.source_refusals.fetch_add(1, SeqCst);
            }
            result
        }
    }
    fn input(
        limits: Limits,
    ) -> (
        tempfile::TempDir,
        init::InitInput,
        Arc<Counted>,
        Arc<Lifecycle>,
        Arc<BackendCount>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("openapi.yaml"),
            include_bytes!("../migrations/native-petstore/openapi.yaml"),
        )
        .unwrap();
        let handler = json!({"defaultHandlers":["spec","validator","proxy"],"handlers":["com.networknt.aws.lambda.handler.middleware.specification.OpenApiMiddleware@spec","com.networknt.aws.lambda.handler.middleware.validator.ValidatorMiddleware@validator","com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy"]});
        for (name, value) in [
            ("handler.yml", handler),
            (
                "values.yml",
                json!({"light-lambda.stage":"validator-lambda-qualification","lambda-proxy.region":"us-east-1","lambda-proxy.functions":{"/v1/pets@post":"qualification-only-business"},"security.enabled":false,"unified-security.enabled":false,"router.enabled":false,"logger.enabled":false}),
            ),
            ("openapi-handler.yml", json!({"enabled":true})),
            (
                "openapi-validator.yml",
                json!({"enabled":true,"skipBodyValidation":false,"handleNullableField":true,"skipPathPrefixes":[]}),
            ),
        ] {
            std::fs::write(dir.path().join(name), value.to_string()).unwrap();
        }
        let lifecycle =
            Lifecycle::create(&dir.path().join("actual-owner-observation.jsonl")).unwrap();
        let engine = Arc::new(Counted {
            engine: entry::profile_engine(lifecycle.clone(), Some(limits)).unwrap(),
            calls: AtomicUsize::new(0),
            source_refusals: AtomicUsize::new(0),
        });
        let mut input = entry::trusted_input(
            Some(PathBuf::from(dir.path())),
            false,
            engine.clone(),
            Duration::from_secs(2),
        )
        .unwrap();
        let backend = Arc::new(BackendCount(AtomicUsize::new(0)));
        input.backend = BackendConfiguration::Injected(backend.clone());
        (dir, input, engine, lifecycle, backend)
    }
    #[test]
    fn candidate_limits_are_explicit_test_only_and_below_existing_parent_envelope() {
        let limits = entry::qualification_limits();
        assert_eq!(limits.max_source_bytes, entry::catalogue_pin().source_bytes);
        assert_eq!(limits.metadata.max_wire_bytes, 262144);
        assert_eq!(limits.metadata.max_peak_bytes, 33554432);
        assert_eq!(limits.metadata.max_retained_bytes, 16777216);
        assert!(limits.metadata.max_retained_bytes <= limits.metadata.max_peak_bytes);
        assert!(limits.metadata.max_peak_bytes < 224 * 1024 * 1024);
    }
    #[test]
    fn missing_limits_cannot_construct_the_observed_accounted_engine() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = Lifecycle::create(&dir.path().join("observer.jsonl")).unwrap();
        let before = Authority::process().phase();
        assert!(matches!(
            entry::profile_engine(lifecycle.clone(), None),
            Err(Failure::AdapterInvariant)
        ));
        assert!(lifecycle.snapshot().children.is_empty());
        assert_eq!(Authority::process().phase(), before);
    }
    #[tokio::test]
    async fn accounted_profile_offline_validation_never_calls_engine_or_backend() {
        let _serial = INIT_CASE.lock().await;
        let (_dir, input, engine, lifecycle, backend) = input(entry::qualification_limits());
        let report = init::validate_configuration(&input).await;
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|s| s.contains("offline validation does not start an engine")),
            "{:?}",
            report.errors
        );
        assert_eq!(engine.calls.load(SeqCst), 0);
        assert_eq!(backend.0.load(SeqCst), 0);
        assert!(lifecycle.snapshot().children.is_empty());
    }
    #[tokio::test]
    async fn real_init_reaches_accounted_source_refusal_before_child_or_backend() {
        let _serial = INIT_CASE.lock().await;
        let mut limits = entry::qualification_limits();
        limits.max_source_bytes = 1;
        let (_dir, input, engine, lifecycle, backend) = input(limits);
        let before = Authority::process().phase();
        assert_eq!(before, Phase::Idle);
        assert!(matches!(
            init::initialize(input).await,
            Err(InitError::InvalidConfiguration(_))
        ));
        assert_eq!(engine.calls.load(SeqCst), 1);
        assert_eq!(engine.source_refusals.load(SeqCst), 1);
        assert_eq!(backend.0.load(SeqCst), 0);
        assert_eq!(Authority::process().phase(), before);
        let snapshot = lifecycle.snapshot();
        assert!(snapshot.children.is_empty());
        assert_eq!(snapshot.active_owners, 0);
    }
    #[tokio::test]
    async fn required_chain_mismatch_precedes_accounted_engine_and_preserves_zero_dispatch() {
        let _serial = INIT_CASE.lock().await;
        let (dir, input, engine, lifecycle, backend) = input(entry::qualification_limits());
        let path = dir.path().join("handler.yml");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["defaultHandlers"] = json!(["spec", "proxy"]);
        std::fs::write(path, value.to_string()).unwrap();
        match init::initialize(input).await {
            Err(InitError::InvalidConfiguration(message)) => assert_eq!(
                message,
                light_lambda_runtime::required_validator_profile::PROFILE_ERROR
            ),
            _ => panic!("expected exact required-profile rejection"),
        }
        assert_eq!(engine.calls.load(SeqCst), 0);
        assert_eq!(backend.0.load(SeqCst), 0);
        assert!(lifecycle.snapshot().children.is_empty());
    }
}
