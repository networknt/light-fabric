//! Bounded best-effort stdout policy records; no transport or provider runtime.
use serde_json::Value;
use std::{
    io::Write,
    sync::{
        OnceLock,
        mpsc::{SyncSender, sync_channel},
    },
};

pub const MAX_RECORD_BYTES: usize = 65536;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Audit {
        error_level: bool,
        stack_trace: bool,
    },
    Emf,
}
pub trait RecordSink: Send + Sync {
    /// No acknowledgment or durability promise. A full/unavailable queue drops.
    fn try_emit(&self, kind: RecordKind, record: Value);
}
#[derive(Default)]
pub struct StdoutSink;
struct Line {
    kind: RecordKind,
    json: String,
}
fn queue() -> Option<&'static SyncSender<Line>> {
    static QUEUE: OnceLock<Option<SyncSender<Line>>> = OnceLock::new();
    QUEUE.get_or_init(||{
        let (sender,receiver)=sync_channel::<Line>(64);
        std::thread::Builder::new().name("lambda-policy-stdout".into()).spawn(move||{
            for line in receiver {
                match line.kind {
                    RecordKind::Audit{error_level:true,stack_trace:true}=>tracing::error!(target:"audit",audit=%line.json,stackTrace="<redacted>"),
                    RecordKind::Audit{error_level:true,stack_trace:false}=>tracing::error!(target:"audit",audit=%line.json),
                    RecordKind::Audit{error_level:false,stack_trace:true}=>tracing::info!(target:"audit",audit=%line.json,stackTrace="<redacted>"),
                    RecordKind::Audit{error_level:false,stack_trace:false}=>tracing::info!(target:"audit",audit=%line.json),
                    RecordKind::Emf=>{
                        let mut stdout=std::io::stdout().lock();
                        let _=stdout.write_all(line.json.as_bytes());
                        let _=stdout.write_all(b"\n");
                    }
                }
            }
        }).ok().map(|_|sender)
    }).as_ref()
}
impl RecordSink for StdoutSink {
    fn try_emit(&self, kind: RecordKind, record: Value) {
        // Concrete builders bound selectors/strings before serialization.
        let Ok(json) = serde_json::to_string(&record) else {
            return;
        };
        if json.len() > MAX_RECORD_BYTES {
            return;
        }
        if let Some(queue) = queue() {
            let _ = queue.try_send(Line { kind, json });
        }
    }
}
