use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::Full;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_json::json;
use std::{fmt, time::Duration};

/// Only static, reviewed diagnostics can cross the Init reporting boundary.
/// Underlying loader/SDK errors and panic payloads are deliberately not retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    InvalidConfiguration(&'static str),
    MissingDirectory(std::path::PathBuf),
    UnknownHandler(String),
    Preparation,
    SecurityBootstrap,
    Backend,
    Panic,
}
impl InitError {
    pub fn message(&self) -> String {
        match self {
            Self::MissingDirectory(path) => {
                let path = path.to_string_lossy();
                let safe = path.len() <= 256 && !path.chars().any(char::is_control);
                return format!(
                    "configuration: required deployment directory missing: {}",
                    if safe { &path } else { "<redacted>" }
                );
            }
            Self::UnknownHandler(class) => {
                let safe = class.len() <= 256
                    && class
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._$".contains(&b));
                return format!(
                    "handler.yml: unknown handler class {}",
                    if safe { class } else { "<redacted>" }
                );
            }
            Self::InvalidConfiguration(message) => message,
            Self::Preparation => "configuration: local preparation failed",
            Self::SecurityBootstrap => "security.yml: bootstrap failed",
            Self::Backend => "lambda-proxy.yml: backend initialization failed",
            Self::Panic => "initialization: panic",
        }
        .to_owned()
    }
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Panic => "Runtime.InitPanic",
            Self::InvalidConfiguration(_) | Self::MissingDirectory(_) | Self::UnknownHandler(_) => {
                "InvalidConfiguration"
            }
            Self::Preparation => "ConfigurationPreparation",
            Self::SecurityBootstrap => "SecurityBootstrap",
            Self::Backend => "BackendInitialization",
        }
    }
    pub fn payload(&self) -> Vec<u8> {
        // Static input may be supplied by framework integrators; cap the entire envelope.
        let mut message = self.message().to_owned();
        loop {
            let bytes =
                serde_json::to_vec(&json!({"errorType":self.error_type(),"errorMessage":message}))
                    .expect("JSON values serialize");
            if bytes.len() <= 1024 {
                return bytes;
            }
            message.pop();
        }
    }
}
impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for InitError {}
pub(crate) type HttpClient = Client<HttpConnector, Full<Bytes>>;
pub(crate) fn client() -> HttpClient {
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build_http()
}
pub(crate) fn endpoint(authority: &str, path: &str) -> Result<Uri, InitError> {
    let authority = authority
        .parse::<http::uri::Authority>()
        .map_err(|_| InitError::InvalidConfiguration("runtime: invalid AWS_LAMBDA_RUNTIME_API"))?;
    if authority.as_str().contains('@') {
        return Err(InitError::InvalidConfiguration(
            "runtime: invalid AWS_LAMBDA_RUNTIME_API",
        ));
    }
    Uri::builder()
        .scheme("http")
        .authority(authority)
        .path_and_query(path)
        .build()
        .map_err(|_| InitError::InvalidConfiguration("runtime: invalid endpoint"))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOutcome {
    MissingEndpoint,
    InvalidEndpoint,
    Accepted,
    Rejected,
    TransportFailure,
    Timeout,
}
/// One attempt at most; no environment mutation, proxy, retry, or error-body logging.
pub async fn report(error: &InitError, authority: Option<&str>) -> ReportOutcome {
    let Some(authority) = authority else {
        return ReportOutcome::MissingEndpoint;
    };
    let Ok(uri) = endpoint(authority, "/2018-06-01/runtime/init/error") else {
        return ReportOutcome::InvalidEndpoint;
    };
    let request = Request::post(uri)
        .header(
            "Lambda-Runtime-Function-Error-Type",
            if matches!(error, InitError::Panic) {
                "Runtime.InitPanic"
            } else {
                "Runtime.InitError"
            },
        )
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(error.payload())))
        .expect("constant headers");
    match tokio::time::timeout(Duration::from_secs(2), client().request(request)).await {
        Err(_) => ReportOutcome::Timeout,
        Ok(Err(_)) => ReportOutcome::TransportFailure,
        Ok(Ok(response)) if response.status().is_success() => ReportOutcome::Accepted,
        Ok(Ok(_)) => ReportOutcome::Rejected,
    }
}
pub async fn report_init_failure(error: &InitError) -> ! {
    let endpoint = std::env::var("AWS_LAMBDA_RUNTIME_API").ok();
    let outcome = report(error, endpoint.as_deref()).await;
    tracing::error!(target:"light_lambda::init", error_type=error.error_type(), message=error.message(), report_outcome=?outcome);
    std::process::exit(1)
}
