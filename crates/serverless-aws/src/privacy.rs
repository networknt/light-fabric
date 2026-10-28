//! Drop unused backend log tails before SDK TRACE sees the HTTP response.
//! This leaves tracing enabled for every target and does not replace SDK TLS,
//! timeout, connection-pool, signing, or credential behavior.
use aws_smithy_runtime_api::client::{
    http::{
        HttpClient, HttpConnector, HttpConnectorFuture, SharedHttpClient, SharedHttpConnector,
        http_client_fn,
    },
    orchestrator::HttpRequest,
};

#[derive(Debug)]
struct NoLogTail(SharedHttpConnector);
impl HttpConnector for NoLogTail {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let call = self.0.call(request);
        HttpConnectorFuture::new(async move {
            let mut response = call.await?;
            response.headers_mut().remove("x-amz-log-result");
            Ok(response)
        })
    }
}
pub(crate) fn no_log_tail(inner: SharedHttpClient) -> SharedHttpClient {
    http_client_fn(move |settings, components| {
        SharedHttpConnector::new(NoLogTail(inner.http_connector(settings, components)))
    })
}

// Credentials' Debug in the pinned SDK deliberately prints access_key_id inside
// runtime_components, identity, endpoint_params and signing_params. Keep every
// event and field, but redact that identifier before forwarding to the caller's
// subscriber. Do not turn off SDK targets or alter the application's subscriber.
use tracing_core::{
    Dispatch, Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Current, Id, Record},
    subscriber::Interest,
};

fn redact(mut text: String) -> String {
    let marker = "access_key_id: \"";
    let mut start = 0;
    while let Some(found) = text[start..].find(marker) {
        let from = start + found + marker.len();
        let mut escaped = false;
        let end = text[from..]
            .char_indices()
            .find_map(|(i, c)| {
                if escaped {
                    escaped = false;
                    return None;
                }
                if c == '\\' {
                    escaped = true;
                    return None;
                }
                (c == '"').then_some(from + i)
            })
            .unwrap_or(text.len());
        text.replace_range(from..end, "<redacted>");
        start = from + "<redacted>".len();
    }
    // SigV4 Authorization credential scope, if a client debug value exposes it.
    let mut start = 0;
    while let Some(found) = text[start..].find("Credential=") {
        let from = start + found + "Credential=".len();
        let end = text[from..]
            .find('/')
            .map(|i| from + i)
            .unwrap_or(text.len());
        text.replace_range(from..end, "<redacted>");
        start = from + "<redacted>".len();
    }
    text
}
#[derive(Default)]
struct Fields(Vec<(Field, String)>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.clone(), redact(format!("{value:?}"))));
    }
}
struct CredentialPrivacy {
    inner: Dispatch,
}
impl Subscriber for CredentialPrivacy {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
    }
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.inner.register_callsite(metadata)
    }
    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        self.inner.new_span(attrs)
    }
    fn record(&self, span: &Id, values: &Record<'_>) {
        self.inner.record(span, values);
    }
    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.inner.record_follows_from(span, follows);
    }
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let displayed: Vec<_> = fields
            .0
            .iter()
            .map(|(_, value)| tracing_core::field::display(value))
            .collect();
        let values: Vec<_> = event
            .metadata()
            .fields()
            .iter()
            .map(|field| {
                fields
                    .0
                    .iter()
                    .position(|(f, _)| *f == field)
                    .map(|i| &displayed[i] as &dyn tracing_core::field::Value)
            })
            .collect();
        let values = event.metadata().fields().value_set_all(&values);
        if event.is_contextual() {
            self.inner.event(&Event::new(event.metadata(), &values));
        } else {
            self.inner.event(&Event::new_child_of(
                event.parent().cloned(),
                event.metadata(),
                &values,
            ));
        }
    }
    fn enter(&self, span: &Id) {
        self.inner.enter(span);
    }
    fn exit(&self, span: &Id) {
        self.inner.exit(span);
    }
    fn clone_span(&self, id: &Id) -> Id {
        self.inner.clone_span(id)
    }
    fn try_close(&self, id: Id) -> bool {
        self.inner.try_close(id)
    }
    fn current_span(&self) -> Current {
        self.inner.current_span()
    }
}
pub(crate) fn credential_safe_dispatch() -> Dispatch {
    tracing_core::dispatcher::get_default(|inner| {
        Dispatch::new(CredentialPrivacy {
            inner: inner.clone(),
        })
    })
}
