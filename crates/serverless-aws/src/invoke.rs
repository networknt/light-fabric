use crate::{
    limits::{Limits, status},
    target::{EndpointOverride, Targets},
    v1::{EventConfig, build_v1_event, parse_response},
};
use async_trait::async_trait;
use aws_sdk_lambda::{
    Client,
    config::{BehaviorVersion, Builder},
    error::{ProvideErrorMetadata, SdkError},
    operation::invoke::InvokeError,
    primitives::Blob,
    types::{InvocationType, LogType},
};
use aws_smithy_runtime_api::client::{
    interceptors::context::InterceptorContext,
    retries::classifiers::{ClassifyRetry, RetryAction, RetryClassifierPriority},
};
use aws_smithy_types::{retry::RetryConfig, timeout::TimeoutConfig};
use aws_types::request_id::RequestId;
use chrono::{DateTime, Utc};
use serverless_core::{
    Backend, DispatchError, ExchangeState, GatewayResponse, Invocation, StatusError, Target,
};
use std::{error::Error, sync::Arc, time::Duration};
use tokio::time::Instant;
use tracing::instrument::WithSubscriber;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Debug, Default)]
pub struct InvokeOptions {
    pub max_retries: u32,
    pub api_call_timeout: Option<Duration>,
    pub api_call_attempt_timeout: Option<Duration>,
    pub connection_timeout: Option<Duration>,
    /// No separate socket read/write knobs in the SDK. These cap operation time.
    pub read_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
}

#[derive(Debug)]
struct SafeRetry;
impl ClassifyRetry for SafeRetry {
    fn classify_retry(&self, ctx: &InterceptorContext) -> RetryAction {
        if let Some(Err(error)) = ctx.output_or_error() {
            if let Some(error) = error
                .as_operation_error()
                .and_then(|e| e.downcast_ref::<InvokeError>())
                && matches!(
                    error.code(),
                    Some(
                        "TooManyRequestsException"
                            | "EC2ThrottledException"
                            | "ENILimitReachedException"
                            | "SubnetIPAddressLimitReachedException"
                    )
                )
            {
                return RetryAction::throttling_error();
            }
            // Only a positively identified connect refusal, with no response,
            // is proof of failure before send. Generic I/O/reset/timeout is not.
            if ctx.response().is_none()
                && let Some(connector) = error.as_connector_error()
            {
                let mut cause: Option<&(dyn Error + 'static)> = Some(connector);
                let mut connect_failure = false;
                let mut refused = false;
                while let Some(error) = cause {
                    connect_failure |= error
                        .downcast_ref::<hyper_util::client::legacy::Error>()
                        .is_some_and(|e| e.is_connect());
                    refused |= error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
                    cause = error.source();
                }
                if connect_failure && refused {
                    return RetryAction::transient_error();
                }
            }
        }
        // This terminates classifier composition, including default 5xx/read-
        // timeout classifiers. NoActionIndicated would NOT override them.
        RetryAction::RetryForbidden
    }
    fn name(&self) -> &'static str {
        "LightLambdaSafeRetry"
    }
    fn priority(&self) -> RetryClassifierPriority {
        RetryClassifierPriority::run_before(RetryClassifierPriority::http_status_code_classifier())
    }
}

pub struct LambdaInvokeBackend {
    client: Client,
    targets: Targets,
    event: EventConfig,
    limits: Limits,
    options: InvokeOptions,
    clock: Arc<dyn Clock>,
}
impl LambdaInvokeBackend {
    /// SDK configuration comes from trusted Init wiring. Tests supply synthetic
    /// static credentials directly; this constructor never loads a provider chain.
    pub fn new(
        config: aws_sdk_lambda::Config,
        targets: Targets,
        event: EventConfig,
        limits: Limits,
        options: InvokeOptions,
        clock: Arc<dyn Clock>,
        endpoint_override: Option<EndpointOverride>,
    ) -> Result<Self, &'static str> {
        limits.validate()?;
        if options.max_retries == u32::MAX
            || [
                options.api_call_timeout,
                options.api_call_attempt_timeout,
                options.connection_timeout,
                options.read_timeout,
                options.write_timeout,
            ]
            .into_iter()
            .flatten()
            .any(|d| d.is_zero())
        {
            return Err("invalid Invoke retry/timeout configuration");
        }
        let http = config.http_client().unwrap_or_else(|| {
            aws_smithy_http_client::Builder::new()
                .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                    aws_smithy_http_client::tls::rustls_provider::CryptoMode::AwsLc,
                ))
                .build_https()
        });
        let mut builder = config
            .to_builder()
            .behavior_version(BehaviorVersion::latest())
            .retry_config(RetryConfig::standard().with_max_attempts(1 + options.max_retries))
            .retry_classifier(SafeRetry)
            .http_client(crate::privacy::no_log_tail(http));
        builder.set_endpoint_url(endpoint_override.map(EndpointOverride::into_url));
        let config = builder.build();
        Ok(Self {
            client: Client::from_conf(config),
            targets,
            event,
            limits,
            options,
            clock,
        })
    }
}

#[async_trait]
impl Backend for LambdaInvokeBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Err(DispatchError(StatusError::timeout()));
        }
        let Target::Function(alias) = target else {
            return Err(DispatchError(StatusError::missing_target()));
        };
        let function = self
            .targets
            .get(alias)
            .ok_or_else(|| DispatchError(StatusError::missing_target()))?;
        let event =
            build_v1_event(inv, state, &self.event, self.clock.now()).map_err(DispatchError)?;
        let payload = serde_json::to_vec(&event).map_err(|_| DispatchError(status(14012, "")))?;
        if payload.len() > self.limits.max_invoke_payload_bytes {
            return Err(DispatchError(status(
                14013,
                self.limits.max_invoke_payload_bytes,
            )));
        }
        let remaining = inv.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || inv.cancel.is_cancelled() {
            return Err(DispatchError(StatusError::timeout()));
        }
        let operation = [
            self.options.api_call_timeout,
            self.options.read_timeout,
            self.options.write_timeout,
        ]
        .into_iter()
        .flatten()
        .fold(remaining, Duration::min);
        let attempt = self
            .options
            .api_call_attempt_timeout
            .unwrap_or(operation)
            .min(operation);
        let connect = self
            .options
            .connection_timeout
            .unwrap_or(attempt)
            .min(attempt);
        let timeout = TimeoutConfig::builder()
            .operation_timeout(operation)
            .operation_attempt_timeout(attempt)
            .connect_timeout(connect)
            .build();
        let call = self
            .client
            .invoke()
            .function_name(function)
            .invocation_type(InvocationType::RequestResponse)
            .log_type(LogType::None)
            .payload(Blob::new(payload))
            .customize()
            .config_override(Builder::new().timeout_config(timeout))
            .send()
            .with_subscriber(crate::privacy::credential_safe_dispatch());
        let result = tokio::select! {
            biased;
            _ = inv.cancel.cancelled() => return Err(DispatchError(StatusError::timeout())),
            result = tokio::time::timeout_at(inv.deadline, call) => result.map_err(|_| DispatchError(StatusError::timeout()))?,
        };
        match result {
            Ok(output) => {
                // API response identity only: execution-ID equivalence needs separate proof.
                let backend_invoke_api_request_id = output.request_id().filter(|id| {
                    id.len() == 36
                        && id.bytes().enumerate().all(|(i, b)| {
                            if [8, 13, 18, 23].contains(&i) {
                                b == b'-'
                            } else {
                                b.is_ascii_hexdigit()
                            }
                        })
                });
                {
                    // Root events alone do not isolate lookup_current in the
                    // pinned formatter. Only this diagnostic enters an empty
                    // root; it never carries caller or credential attributes.
                    let root = tracing::info_span!(parent: None, "safe_diagnostic");
                    let _entered = root.enter();
                    tracing::info!(parent: None, event="invoke_response", request_id=inv.request_id,
                        target_alias=alias, backend_invoke_api_request_id);
                }
                parse_response(
                    output.payload().map(|p| p.as_ref()).unwrap_or_default(),
                    output.function_error(),
                    output.executed_version(),
                    &inv.request_id,
                    alias,
                )
                .map_err(DispatchError)
            }
            Err(error) => {
                let code = match &error {
                    SdkError::TimeoutError(_) => 14017,
                    SdkError::DispatchFailure(e) if e.is_timeout() => 14017,
                    _ if error.as_service_error().and_then(|e| e.code())
                        == Some("TooManyRequestsException") =>
                    {
                        14018
                    }
                    _ if error
                        .as_service_error()
                        .and_then(|e| e.code())
                        .is_some_and(|c| {
                            matches!(
                                c,
                                "ResourceNotFoundException"
                                    | "AccessDenied"
                                    | "AccessDeniedException"
                            )
                        })
                        || error
                            .raw_response()
                            .is_some_and(|r| r.status().as_u16() == 403) =>
                    {
                        14019
                    }
                    _ => 14014,
                };
                tracing::warn!(
                    request_id = inv.request_id,
                    target_alias = alias,
                    status_code = code
                );
                Err(DispatchError(status(code, alias)))
            }
        }
    }
}
