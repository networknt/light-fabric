//! Shared configuration/security clients emit value-bearing diagnostics. Retain
//! their enabled events, levels, targets and field names, replacing field values
//! before forwarding. Reviewed framework/adapter events keep approved metadata.
use tracing_core::{
    Dispatch, Event, Metadata, Subscriber,
    span::{Attributes, Current, Id, Record},
    subscriber::Interest,
};
pub(crate) struct Privacy(pub Dispatch);
static DIAGNOSTIC_CALLSITE: tracing_core::callsite::DefaultCallsite =
    tracing_core::callsite::DefaultCallsite::new(&DIAGNOSTIC_METADATA);
static DIAGNOSTIC_METADATA: Metadata<'static> = tracing_core::metadata! {
    name: "safe_diagnostic",
    target: "light_security::jwks_fetch",
    level: tracing_core::Level::INFO,
    fields: &[],
    callsite: &DIAGNOSTIC_CALLSITE,
    kind: tracing_core::metadata::Kind::SPAN,
};
impl Subscriber for Privacy {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        self.0.enabled(m)
    }
    fn register_callsite(&self, m: &'static Metadata<'static>) -> Interest {
        self.0.register_callsite(m)
    }
    fn new_span(&self, a: &Attributes<'_>) -> Id {
        self.0.new_span(a)
    }
    fn record(&self, id: &Id, r: &Record<'_>) {
        self.0.record(id, r)
    }
    fn record_follows_from(&self, id: &Id, f: &Id) {
        self.0.record_follows_from(id, f)
    }
    fn event(&self, event: &Event<'_>) {
        let t = event.metadata().target();
        // The pinned JSON formatter consults lookup_current even for a root
        // event. Give only this new domain an empty root span so neither the
        // contextual nor explicit parent's values can be imported. RAII restores
        // the caller's stack, without awaits or changing other event domains.
        let neutral = (t == "light_security::jwks_fetch").then(|| {
            let values = DIAGNOSTIC_METADATA.fields().value_set_all(&[]);
            tracing::Span::new_root_with(&DIAGNOSTIC_METADATA, &values, &self.0)
        });
        let _entered = neutral.as_ref().map(tracing::Span::enter);
        if t == "light_security::jwks_fetch"
            && let Some((kind, id)) = safe_jwks_event(event)
        {
            // Approved fields must not import an enclosing dependency span's
            // token, URL or payload when a consumer enables span rendering.
            let values: Vec<Option<&dyn tracing_core::field::Value>> = event
                .metadata()
                .fields()
                .iter()
                .map(|field| match field.name() {
                    "event" => Some(&kind as &dyn tracing_core::field::Value),
                    "fetch_id" => Some(&id as &dyn tracing_core::field::Value),
                    _ => None,
                })
                .collect();
            let values = event.metadata().fields().value_set_all(&values);
            self.0
                .event(&Event::new_child_of(None::<Id>, event.metadata(), &values));
            return;
        }
        if t == "audit" || t.starts_with("light_lambda::") || t.starts_with("serverless_aws::") {
            self.0.event(event);
            return;
        }
        let redacted = tracing_core::field::display("<redacted dependency diagnostic>");
        let fields: Vec<Option<&dyn tracing_core::field::Value>> = event
            .metadata()
            .fields()
            .iter()
            .map(|_| Some(&redacted as &dyn tracing_core::field::Value))
            .collect();
        let values = event.metadata().fields().value_set_all(&fields);
        self.0.event(&Event::new_child_of(
            if t == "light_security::jwks_fetch" {
                None
            } else {
                event.parent().cloned()
            },
            event.metadata(),
            &values,
        ));
    }
    fn enter(&self, id: &Id) {
        self.0.enter(id)
    }
    fn exit(&self, id: &Id) {
        self.0.exit(id)
    }
    fn clone_span(&self, id: &Id) -> Id {
        self.0.clone_span(id)
    }
    fn try_close(&self, id: Id) -> bool {
        self.0.try_close(id)
    }
    fn current_span(&self) -> Current {
        self.0.current_span()
    }
}

// Unknown fields/types or values retain ordinary dependency redaction.
fn safe_jwks_event(event: &Event<'_>) -> Option<(&'static str, u64)> {
    struct Fields {
        valid: bool,
        kind: Option<&'static str>,
        id: Option<u64>,
    }
    impl tracing_core::field::Visit for Fields {
        fn record_str(&mut self, field: &tracing_core::field::Field, value: &str) {
            self.kind = if field.name() == "event" {
                match value {
                    "started" => Some("started"),
                    "completed" => Some("completed"),
                    "interrupted_or_failed" => Some("interrupted_or_failed"),
                    _ => None,
                }
            } else {
                None
            };
            self.valid &= self.kind.is_some();
        }
        fn record_u64(&mut self, field: &tracing_core::field::Field, value: u64) {
            if field.name() == "fetch_id" {
                self.id = Some(value);
            } else {
                self.valid = false;
            }
        }
        fn record_debug(&mut self, _: &tracing_core::field::Field, _: &dyn std::fmt::Debug) {
            self.valid = false;
        }
    }
    let mut fields = Fields {
        valid: true,
        kind: None,
        id: None,
    };
    event.record(&mut fields);
    if fields.valid && event.metadata().fields().len() == 2 {
        Some((fields.kind?, fields.id?))
    } else {
        None
    }
}

#[cfg(test)]
#[path = "log_privacy_tests.rs"]
mod tests;
