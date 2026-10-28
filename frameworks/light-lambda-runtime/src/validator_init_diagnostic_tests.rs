fn record(stage: Stage, failure: Option<&Failure>) {
    let diagnostic = InitDiagnostic::default();
    diagnostic.note(stage, failure);
    diagnostic.emit_after_run();
}
use super::*;
use crate::log_privacy::Privacy;
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
fn capture(
    level: tracing::level_filters::LevelFilter,
    emit: impl FnOnce(),
) -> (String, Vec<Value>) {
    let writer = Capture::default();
    let inner = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_max_level(level)
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(writer.clone())
        .finish();
    tracing::subscriber::with_default(Privacy(tracing_core::Dispatch::new(inner)), emit);
    let text = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
    let rows = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    (text, rows)
}
#[test]
fn finite_failure_vocabulary_contains_no_error_payload() {
    let failures = [
        None,
        Some(Failure::Frame),
        Some(Failure::ParentBudget),
        Some(Failure::RequestedQuota),
        Some(Failure::AllocatorRefused),
        Some(Failure::EngineResource),
        Some(Failure::EngineInternal),
        Some(Failure::AdapterInvariant),
        Some(Failure::Protocol),
        Some(Failure::Deadline),
        Some(Failure::Cancelled),
        Some(Failure::Death),
        Some(Failure::NoReady),
        Some(Failure::Rate),
        Some(Failure::Stale),
    ];
    let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        for f in &failures {
            record(Stage::EngineInitialize, f.as_ref());
        }
    });
    assert_eq!(rows.len(), failures.len());
    for (row, f) in rows.iter().zip(failures) {
        assert_eq!(
            row["fields"],
            json!({"stage":"engine_initialize","failure":failure_label(f.as_ref())})
        );
    }
}
#[test]
fn surrounding_span_values_never_enter_the_diagnostic_and_context_is_restored() {
    for level in [
        tracing::level_filters::LevelFilter::INFO,
        tracing::level_filters::LevelFilter::DEBUG,
        tracing::level_filters::LevelFilter::TRACE,
    ] {
        let (text, rows) = capture(level, || {
            let outer = tracing::info_span!(
                "PRIVATE_PARENT",
                authorization = "PRIVATE_TOKEN",
                source = "PRIVATE_SCHEMA"
            );
            let _outer = outer.enter();
            let inner = tracing::info_span!(parent:&outer,"PRIVATE_CHILD",payload="PRIVATE_PAYLOAD",error="PRIVATE_ERROR");
            let _inner = inner.enter();
            let before = tracing::Span::current().id();
            record(Stage::MetadataMissing, None);
            record(Stage::MetadataReservation, Some(&Failure::AdapterInvariant));
            assert_eq!(tracing::Span::current().id(), before);
        });
        assert_eq!(rows.len(), 2);
        assert!(!text.contains("PRIVATE_"), "{text}");
        for row in rows {
            assert_eq!(row["span"], json!({"name":"safe_validator_init"}));
            assert_eq!(row["spans"], json!([{"name":"safe_validator_init"}]));
            assert_eq!(row["fields"].as_object().unwrap().len(), 2);
        }
    }
}
#[test]
fn disabled_logging_is_not_bypassed() {
    let (text, rows) = capture(tracing::level_filters::LevelFilter::OFF, || {
        record(Stage::EngineInitialize, Some(&Failure::Protocol))
    });
    assert!(text.is_empty());
    assert!(rows.is_empty());
}
#[test]
fn absent_typed_cause_is_explicit_and_not_an_invented_failure() {
    let (_, rows) = capture(tracing::level_filters::LevelFilter::INFO, || {
        record(Stage::EngineTimeout, None)
    });
    assert_eq!(
        rows[0]["fields"],
        json!({"stage":"engine_timeout","failure":"not_available"})
    );
}
#[test]
fn payload_error_stack_and_log_fields_are_never_emitted() {
    let (_text, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        record(Stage::MetadataTaskJoin, None)
    });
    assert_eq!(
        rows[0]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["failure", "stage"]
    );
    for forbidden in [
        "message",
        "cause",
        "payload",
        "authorization",
        "headers",
        "stack",
        "log_tail",
    ] {
        assert!(rows[0]["fields"].get(forbidden).is_none());
    }
}
#[test]
fn audit_and_existing_jwks_domains_keep_their_original_behavior() {
    let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        record(Stage::MetadataMissing, None);
        tracing::info!(target:"audit",approved_audit_field="retained");
        tracing::info!(target:"light_security::jwks_fetch",event="started",fetch_id=7u64);
    });
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1]["fields"]["approved_audit_field"], "retained");
    assert_eq!(rows[2]["fields"], json!({"event":"started","fetch_id":7}));
}

#[test]
fn note_does_not_emit_and_first_failure_is_preserved_per_init() {
    let diagnostic = InitDiagnostic::default();
    let failure = Failure::Protocol;
    let (text, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        diagnostic.note(Stage::EngineInitialize, Some(&failure));
        diagnostic.note(Stage::MetadataMissing, None);
    });
    assert!(text.is_empty());
    assert!(rows.is_empty());
    assert!(matches!(failure, Failure::Protocol));
    let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        diagnostic.emit_after_run()
    });
    assert_eq!(
        rows[0]["fields"],
        json!({"stage":"engine_initialize","failure":"protocol"})
    );
    let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        InitDiagnostic::default().emit_after_run()
    });
    assert!(rows.is_empty());
}
#[test]
fn concurrent_first_failure_is_an_atomic_pair() {
    let diagnostic = Arc::new(InitDiagnostic::default());
    std::thread::scope(|scope| {
        for i in 0..16 {
            let diagnostic = diagnostic.clone();
            scope.spawn(move || {
                if i % 2 == 0 {
                    diagnostic.note(Stage::EngineInitialize, Some(&Failure::Protocol));
                } else {
                    diagnostic.note(Stage::MetadataReservation, Some(&Failure::RequestedQuota));
                }
            });
        }
    });
    let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
        diagnostic.emit_after_run()
    });
    assert_eq!(rows.len(), 1);
    assert!(
        [
            json!({"stage":"engine_initialize","failure":"protocol"}),
            json!({"stage":"metadata_reservation","failure":"requested_quota"})
        ]
        .contains(&rows[0]["fields"])
    );
}

#[test]
fn io_classes_preserve_finite_distinctions_and_never_format_errors() {
    let cases = [
        (libc::ENOENT, "no_ready_not_found"),
        (libc::ENOTDIR, "no_ready_not_found"),
        (libc::EACCES, "no_ready_permission_denied"),
        (libc::EPERM, "no_ready_permission_denied"),
        (libc::ENOSYS, "no_ready_unsupported"),
        (libc::EOPNOTSUPP, "no_ready_unsupported"),
        (libc::EINVAL, "no_ready_invalid_argument"),
        (libc::EMFILE, "no_ready_descriptor_limit"),
        (libc::ENFILE, "no_ready_descriptor_limit"),
        (libc::ENOMEM, "no_ready_resource"),
        (libc::ENOSPC, "no_ready_resource"),
        (libc::EINTR, "no_ready_interrupted"),
        (libc::EIO, "no_ready_other_io"),
    ];
    for (errno, expected) in cases {
        let d = InitDiagnostic::default();
        let error = std::io::Error::from_raw_os_error(errno);
        let (text, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
            d.note_io(Stage::ImageMemfdCreate, &error)
        });
        assert!(text.is_empty());
        assert!(rows.is_empty());
        d.note(Stage::EngineInitialize, Some(&Failure::NoReady));
        let (_, rows) = capture(tracing::level_filters::LevelFilter::TRACE, || {
            d.emit_after_run()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["fields"],
            json!({"stage":"image_memfd_create","failure":expected})
        );
        assert_eq!(error.raw_os_error(), Some(errno));
    }
}
#[test]
fn opaque_io_text_and_surrounding_context_are_not_retained_or_exported() {
    for level in [
        tracing::level_filters::LevelFilter::INFO,
        tracing::level_filters::LevelFilter::DEBUG,
        tracing::level_filters::LevelFilter::TRACE,
    ] {
        let d = InitDiagnostic::default();
        let error =
            std::io::Error::other("PRIVATE_ERROR_PAYLOAD /PRIVATE_PATH token=PRIVATE_TOKEN");
        let (text, rows) = capture(level, || {
            let span = tracing::info_span!(
                "PRIVATE_PARENT",
                payload = "PRIVATE_PAYLOAD",
                authorization = "PRIVATE_AUTH"
            );
            let _entered = span.enter();
            d.note_io(Stage::ImageOpen, &error);
            d.emit_after_run();
        });
        assert!(!text.contains("PRIVATE_"));
        assert_eq!(
            rows[0]["fields"],
            json!({"stage":"image_open","failure":"no_ready_other_io"})
        );
        assert_eq!(rows[0]["fields"].as_object().unwrap().len(), 2);
    }
}
#[test]
fn refusal_vocabulary_and_instance_isolation_remain_finite() {
    use crate::validator_ipc::supervision::Refusal;
    for (refusal, label) in [
        (Refusal::Busy, "no_ready_busy"),
        (Refusal::Closed, "no_ready_closed"),
        (Refusal::InvalidPolicy, "no_ready_invalid_policy"),
    ] {
        let d = InitDiagnostic::default();
        d.note_refusal(Stage::ControllerOwnerReserve, refusal);
        let (_, rows) = capture(tracing::level_filters::LevelFilter::INFO, || {
            d.emit_after_run()
        });
        assert_eq!(
            rows[0]["fields"],
            json!({"stage":"controller_owner_reserve","failure":label})
        );
    }
    let (_, rows) = capture(tracing::level_filters::LevelFilter::INFO, || {
        InitDiagnostic::default().emit_after_run()
    });
    assert!(rows.is_empty());
}
#[test]
fn concurrent_detailed_first_failure_keeps_operation_and_class_together() {
    let d = Arc::new(InitDiagnostic::default());
    std::thread::scope(|scope| {
        for i in 0..16 {
            let d = d.clone();
            scope.spawn(move || {
                if i % 2 == 0 {
                    d.note_io(
                        Stage::ImageMemfdCreate,
                        &std::io::Error::from_raw_os_error(libc::EINVAL),
                    );
                } else {
                    d.note_io(
                        Stage::ImageOpen,
                        &std::io::Error::from_raw_os_error(libc::ENOENT),
                    );
                }
            });
        }
    });
    d.note(Stage::EngineInitialize, Some(&Failure::NoReady));
    let (_, rows) = capture(tracing::level_filters::LevelFilter::INFO, || {
        d.emit_after_run()
    });
    assert_eq!(rows.len(), 1);
    assert!(
        [
            json!({"stage":"image_memfd_create","failure":"no_ready_invalid_argument"}),
            json!({"stage":"image_open","failure":"no_ready_not_found"})
        ]
        .contains(&rows[0]["fields"])
    );
}
#[test]
fn detailed_notes_still_respect_off_filter() {
    let d = InitDiagnostic::default();
    d.note_io(
        Stage::ImageSealAdd,
        &std::io::Error::from_raw_os_error(libc::EPERM),
    );
    let (text, rows) = capture(tracing::level_filters::LevelFilter::OFF, || {
        d.emit_after_run()
    });
    assert!(text.is_empty());
    assert!(rows.is_empty());
}
