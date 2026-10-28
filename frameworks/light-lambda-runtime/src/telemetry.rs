use crate::init_error::InitError;
use std::sync::{Mutex, Once, OnceLock};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

struct InstalledFilter {
    startup: String,
    setter_admitted: bool,
    current: Mutex<Option<DiagnosticLevel>>,
    reload: Box<dyn Fn(EnvFilter) -> Result<(), ()> + Send + Sync>,
}
static INSTALLED_FILTER: OnceLock<InstalledFilter> = OnceLock::new();
pub fn installed_filter() -> Option<&'static str> {
    let installed = INSTALLED_FILTER.get()?;
    let current = installed.current.lock().unwrap_or_else(|p| p.into_inner());
    Some(match *current {
        Some(level) => level.filter(),
        None => installed.startup.as_str(),
    })
}

/// Finite diagnostic-only domain: no target/field directives or audit suppression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticLevel {
    Info,
    Debug,
    Trace,
}
impl DiagnosticLevel {
    pub(crate) fn command(value: &str) -> Option<Self> {
        match value {
            "INFO" => Some(Self::Info),
            "DEBUG" => Some(Self::Debug),
            "TRACE" => Some(Self::Trace),
            _ => None,
        }
    }
    fn filter(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}
fn startup_filter() -> (EnvFilter, bool) {
    let parsed = EnvFilter::try_from_default_env();
    let explicit_valid = parsed.is_ok() || std::env::var_os("RUST_LOG").is_none();
    let filter = parsed.unwrap_or_else(|_| EnvFilter::new("info"));
    let admitted =
        explicit_valid && matches!(filter.to_string().as_str(), "info" | "debug" | "trace");
    (filter, admitted)
}
pub(crate) fn diagnostic_profile_admitted() -> bool {
    INSTALLED_FILTER
        .get()
        .map_or_else(|| startup_filter().1, |installed| installed.setter_admitted)
}
pub(crate) fn set_diagnostic_level(level: DiagnosticLevel) -> Result<(), ()> {
    let installed = INSTALLED_FILTER.get().ok_or(())?;
    if !installed.setter_admitted {
        return Err(());
    }
    let mut current = installed.current.lock().unwrap_or_else(|p| p.into_inner());
    // Readers and competing updates share this lock. A failed reload publishes nothing.
    (installed.reload)(EnvFilter::new(level.filter()))?;
    *current = Some(level);
    Ok(())
}

/// One process-owned stdout subscriber; tests call this only in isolated children.
pub fn initialize() -> Result<(), InitError> {
    let (filter, setter_admitted) = startup_filter();
    let installed = filter.to_string();
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let subscriber = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(std::io::stdout),
    );
    tracing::subscriber::set_global_default(crate::log_privacy::Privacy(
        tracing_core::Dispatch::new(subscriber),
    ))
    .map_err(|_| InitError::InvalidConfiguration("logging: subscriber already installed"))?;
    // Publish only after successful installation; failed repeated Init cannot
    // change the reported process-owned filter or its privacy wrapper.
    let _ = INSTALLED_FILTER.set(InstalledFilter {
        startup: installed,
        setter_admitted,
        current: Mutex::new(None),
        reload: Box::new(move |filter| handle.reload(filter).map_err(|_| ())),
    });
    install_panic_redaction();
    Ok(())
}
/// A process-lifetime safety boundary, installed before any Init work. No unwind
/// payload reaches the default stderr hook. Never swapped around concurrent tests.
pub fn install_panic_redaction() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|_| {
            tracing::error!(target:"light_lambda::init", message="panic payload redacted");
        }))
    });
}
#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub route: Option<String>,
    pub target: Option<String>,
    pub dispatch_ms: u64,
    pub finalizations: u32,
}
pub(crate) fn bounded(s: &str) -> &str {
    if s.len() > 1024 { "<truncated>" } else { s }
}
pub(crate) struct Record<'a> {
    pub request_id: &'a str,
    pub completion: &'a Completion,
    pub status: u16,
    pub outcome: &'a str,
    pub gateway_ms: u64,
    pub cold_start: bool,
    pub config_digest: &'a str,
    pub layer_identity: Option<&'a str>,
}
pub(crate) fn invocation(r: Record<'_>) {
    tracing::info!(target:"audit", request_id=bounded(r.request_id), route=r.completion.route.as_deref().map(bounded),
        target=r.completion.target.as_deref().map(bounded), status=r.status, outcome=r.outcome,
        gateway_ms=r.gateway_ms, dispatch_ms=r.completion.dispatch_ms, cold_start=r.cold_start, finalizations=r.completion.finalizations,
        config_digest=r.config_digest, adapter_version=env!("CARGO_PKG_VERSION"), layer_identity=r.layer_identity);
}
