use light_lambda_runtime::{
    init::{Environment, HandlerRegistration, InitInput},
    init_error::InitError,
    service::RequestGuard,
};
use serverless_aws::target::GatewayIdentity;
use serverless_core::{HandlerId, RouteMatch, StatusError};
use std::{path::PathBuf, sync::Arc};

pub mod validator_deployment_profile;
pub mod validator_deployment_runtime;

pub mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run,
    Validate(Option<PathBuf>),
    Print,
    Dump(PathBuf),
}
pub fn parse_args(args: &[String]) -> Result<Command, &'static str> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["run"] => Ok(Command::Run),
        ["validate-config"] => Ok(Command::Validate(None)),
        ["validate-config", "--config-dir", dir] if !dir.is_empty() && !dir.starts_with('-') => {
            Ok(Command::Validate(Some(PathBuf::from(dir))))
        }
        ["--print-embedded-config"] => Ok(Command::Print),
        ["--dump-embedded-configs", dir] if !dir.is_empty() && !dir.starts_with('-') => {
            Ok(Command::Dump(PathBuf::from(dir)))
        }
        _ => Err(
            "usage: bootstrap [run | validate-config [--config-dir <dir>] | --print-embedded-config | --dump-embedded-configs <dir>]",
        ),
    }
}

/// Phase-1 reservation, applied to the same canonical path selected by the core.
/// A single nonempty cache-name segment matches {name}; extra/trailing segments do not.
pub fn reserved_path(path: &str) -> Option<&'static str> {
    match path {
        "/health" => Some("/health"),
        "/adm/health" => Some("/adm/health"),
        "/adm/server/info" => Some("/adm/server/info"),
        "/adm/logger" => Some("/adm/logger"),
        _ => path
            .strip_prefix("/adm/cache/")
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .map(|_| "/adm/cache/{name}"),
    }
}
pub fn phase1_guard() -> RequestGuard {
    Arc::new(|inv| {
        let path = reserved_path(&inv.path)?;
        // The runtime additionally verifies the compiled local terminal. Until
        // qualified factories/routes exist these GETs still return local404.
        // The runtime reservation guard first proves the compiled local terminal
        // and JWT/Security chain. This callback cannot authorize a proxy target.
        if (inv.method == "GET" && path != "/adm/cache/{name}")
            || (inv.method == "POST" && path == "/adm/logger")
        {
            return None;
        }
        inv.route = Some(RouteMatch {
            template: path.into(),
            target: None,
        });
        Some(
            StatusError::new(
                404,
                "ERR10007",
                "INVALID_REQUEST_PATH",
                &format!("Request path {path} cannot be found in the spec."),
            )
            .response(),
        )
    })
}

/// Function ARN is trusted deployment identity, never derived from an invocation.
/// Offline validation uses a synthetic name only when no identity was supplied;
/// its report explicitly does not establish deployment target readiness.
pub fn input(dir: Option<PathBuf>, running: bool) -> Result<InitInput, InitError> {
    let logger_set =
        light_lambda_runtime::logger_set_policy::LoggerSetPolicyProvider::from_startup_gate()?;
    let environment = if running {
        Environment::from_env()?
    } else {
        Environment {
            region: Some("us-east-1".into()),
            function_name: "offline-validator".into(),
            function_version: "offline".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: std::env::var(
                "LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS",
            )
            .as_deref()
                == Ok("1"),
        }
    };
    let arn = match std::env::var("LIGHT_LAMBDA_GATEWAY_ARN") {
        Ok(arn) => arn,
        Err(_) if !running => {
            "arn:aws:lambda:us-east-1:000000000000:function:offline-validator".into()
        }
        Err(_) => {
            return Err(InitError::InvalidConfiguration(
                "environment: LIGHT_LAMBDA_GATEWAY_ARN required for target identity validation",
            ));
        }
    };
    let pieces: Vec<_> = arn.split(':').collect();
    let ["arn", _, "lambda", region, account, "function", name, ..] = pieces.as_slice() else {
        return Err(InitError::InvalidConfiguration(
            "environment: invalid LIGHT_LAMBDA_GATEWAY_ARN",
        ));
    };
    let identity = GatewayIdentity {
        function_name: (*name).into(),
        region: (*region).into(),
        account_id: (*account).into(),
    };
    let environment = if running {
        environment
    } else {
        Environment {
            region: Some(identity.region.clone()),
            function_name: identity.function_name.clone(),
            ..environment
        }
    };
    let mut input = if running {
        InitInput::from_env(embedded_config::FILES, identity)?
    } else {
        InitInput {
            embedded: embedded_config::FILES,
            local_dir: "config".into(),
            default_dir: None,
            external_dir: dir
                .clone()
                .or_else(|| std::env::var_os("LIGHT_LAMBDA_CONFIG_DIR").map(PathBuf::from))
                .unwrap_or_else(|| "/opt".into()),
            environment: environment.clone(),
            gateway_identity: identity,
            handlers: vec![],
            finalizers: vec![],
            backend: light_lambda_runtime::init::BackendConfiguration::Offline,
        }
    };
    input.environment = environment;
    if let Some(dir) = dir {
        input.external_dir = dir;
    }
    for (id, file) in [
        (HandlerId::Metrics, "metrics.yml"),
        (HandlerId::Limit, "limit.yml"),
        (HandlerId::Traceability, "traceability.yml"),
        (HandlerId::Correlation, "correlation.yml"),
        (HandlerId::RequestCors, "cors.yml"),
        (HandlerId::ResponseCors, "cors.yml"),
        (HandlerId::RequestHeader, "header.yml"),
        (HandlerId::ResponseHeader, "header.yml"),
        (HandlerId::RequestTransformer, "request-transformer.yml"),
        (HandlerId::ResponseTransformer, "response-transformer.yml"),
        (HandlerId::Audit, "audit.yml"),
        (HandlerId::Token, "token.yml"),
        (HandlerId::Router, "router.yml"),
        (HandlerId::Specification, "openapi-handler.yml"),
        (HandlerId::Sanitizer, "sanitizer.yml"),
        (HandlerId::Validator, "openapi-validator.yml"),
        (HandlerId::Swt, "swt.yml"),
        (HandlerId::Basic, "basic-auth.yml"),
        (HandlerId::ApiKey, "apikey.yml"),
        (HandlerId::Health, "health.yml"),
        (HandlerId::Info, "info.yml"),
        (HandlerId::GetLogger, "logging.yml"),
        (HandlerId::SetLogger, "logging.yml"),
        (HandlerId::Cache, "cache.yml"),
    ] {
        input.handlers.push(HandlerRegistration {
            id,
            config_file: file,
            factory: match id {
                HandlerId::SetLogger => logger_set.clone().map(|p| {
                    Arc::new(p) as Arc<dyn light_lambda_runtime::init::HandlerFactoryProvider>
                }),
                HandlerId::Router => Some(Arc::new(
                    light_lambda_runtime::router_policy::ApplicationRouterPolicyProvider,
                )),
                HandlerId::Validator => Some(Arc::new(
                    light_lambda_runtime::validator_policy::ValidatorPolicyProvider::default(),
                )),
                HandlerId::RequestTransformer => Some(Arc::new(
                    light_lambda_runtime::transformer_policy::RequestTransformerPolicyProvider,
                )),
                HandlerId::ResponseTransformer => Some(Arc::new(
                    light_lambda_runtime::transformer_policy::ResponseTransformerPolicyProvider,
                )),
                // Day-one protected admin access requires a verified OAuth/JWT
                // principal. Non-JWT admin compatibility is owner-deferred.
                HandlerId::Health | HandlerId::Info | HandlerId::GetLogger => Some(Arc::new(
                    light_lambda_runtime::admin_policy::AdminPolicyProvider(id),
                )),
                HandlerId::Audit => Some(Arc::new(
                    light_lambda_runtime::audit_policy::AuditPolicyProvider,
                )),
                HandlerId::Sanitizer => Some(Arc::new(
                    light_lambda_runtime::sanitizer_policy::SanitizerPolicyProvider,
                )),
                HandlerId::Specification => Some(Arc::new(
                    light_lambda_runtime::specification_policy::SpecificationPolicyProvider,
                )),
                HandlerId::Metrics => Some(Arc::new(
                    light_lambda_runtime::metrics_policy::MetricsPolicyProvider {
                        function_name: input.environment.function_name.clone(),
                    },
                )),
                HandlerId::Token => Some(Arc::new(
                    light_lambda_runtime::token_policy::TokenPolicyProvider,
                )),
                HandlerId::Limit => Some(Arc::new(
                    light_lambda_runtime::limit_policy::LimitPolicyProvider,
                )),
                HandlerId::RequestCors => Some(Arc::new(
                    light_lambda_runtime::cors_policy::CorsPolicyProvider(false),
                )),
                HandlerId::ResponseCors => Some(Arc::new(
                    light_lambda_runtime::cors_policy::CorsPolicyProvider(true),
                )),
                HandlerId::Basic => Some(Arc::new(
                    light_lambda_runtime::factor_policy::FactorPolicyProvider::Basic,
                )),
                HandlerId::ApiKey => Some(Arc::new(
                    light_lambda_runtime::factor_policy::FactorPolicyProvider::ApiKey,
                )),
                HandlerId::Correlation => Some(Arc::new(
                    light_lambda_runtime::head_policy::HeadPolicyProvider::Correlation,
                )),
                HandlerId::Traceability => Some(Arc::new(
                    light_lambda_runtime::head_policy::HeadPolicyProvider::Traceability,
                )),
                HandlerId::RequestHeader => Some(Arc::new(
                    light_lambda_runtime::head_policy::HeadPolicyProvider::RequestHeader,
                )),
                HandlerId::ResponseHeader => Some(Arc::new(
                    light_lambda_runtime::head_policy::HeadPolicyProvider::ResponseHeader,
                )),
                _ => None,
            },
        });
    }
    Ok(input)
}

/// Explicit local qualification composition. Default input/activation is not
/// changed; executable/profile admission is a separate owner-reviewed package.
pub fn input_with_precision_engine(
    dir: Option<PathBuf>,
    running: bool,
    engine: Arc<dyn light_lambda_runtime::validator_ipc::Engine>,
    init_timeout: std::time::Duration,
) -> Result<InitInput, InitError> {
    let mut input = input(dir, running)?;
    let provider = Arc::new(
        light_lambda_runtime::validator_policy::ValidatorPolicyProvider::with_engine(
            engine,
            Arc::new(light_lambda_runtime::validator_failure_policy::ValidatorFailureDisposition),
            init_timeout,
        ),
    );
    for id in [HandlerId::Specification, HandlerId::Validator] {
        let declaration = input.handlers.iter_mut().find(|r| r.id == id).ok_or(
            InitError::InvalidConfiguration("validator: application registration missing"),
        )?;
        declaration.factory = Some(provider.clone());
    }
    Ok(input)
}

/// Explicit in-process Validator composition. No child or worker bundle is used.
/// Ordinary input and the accepted bootstrap keep their existing activation gate.
pub fn input_with_in_process_validator(
    dir: Option<PathBuf>,
    running: bool,
    limits: light_lambda_runtime::validator_ipc::in_process::Limits,
    init_timeout: std::time::Duration,
) -> Result<InitInput, InitError> {
    let engine = light_lambda_runtime::validator_ipc::in_process::InProcessEngine::new(limits)
        .map_err(|_| InitError::InvalidConfiguration("validator: invalid in-process limits"))?;
    let mut input = input(dir, running)?;
    let provider = Arc::new(
        light_lambda_runtime::validator_policy::ValidatorPolicyProvider::with_engine(
            Arc::new(engine),
            Arc::new(light_lambda_runtime::validator_ipc::in_process::InProcessFailureDisposition),
            init_timeout,
        ),
    );
    let specification = Arc::new(
        light_lambda_runtime::validator_ipc::in_process::SpecificationProvider::new(
            provider.clone(),
        ),
    );
    for (id, factory) in [
        (
            HandlerId::Specification,
            specification as Arc<dyn light_lambda_runtime::init::HandlerFactoryProvider>,
        ),
        (
            HandlerId::Validator,
            provider as Arc<dyn light_lambda_runtime::init::HandlerFactoryProvider>,
        ),
    ] {
        input
            .handlers
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or(InitError::InvalidConfiguration(
                "validator: application registration missing",
            ))?
            .factory = Some(factory);
    }
    Ok(input)
}

/// Explicit source-selected Router qualification. Ordinary input remains
/// forwarding-only; no layer/environment value can select a child executable.
pub fn input_with_router_qualification(
    dir: Option<PathBuf>,
    running: bool,
    factory: Arc<dyn light_lambda_runtime::router_rewrite_policy::ControllerFactory>,
    init_timeout: std::time::Duration,
) -> Result<InitInput, InitError> {
    let mut input = input(dir, running)?;
    let provider =
        light_lambda_runtime::router_rewrite_policy::QualificationRouterPolicyProvider::new(
            factory,
            init_timeout,
        )?;
    input
        .handlers
        .iter_mut()
        .find(|r| r.id == HandlerId::Router)
        .ok_or(InitError::InvalidConfiguration(
            "router: application registration missing",
        ))?
        .factory = Some(Arc::new(provider));
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_rejects_malformed_arguments() {
        for args in [
            vec!["run", "oops"],
            vec!["validate-config", "--config-dir"],
            vec!["--dump-embedded-configs"],
            vec!["unknown"],
            vec!["--print-embedded-config", "oops"],
        ] {
            assert!(parse_args(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
    }
    #[test]
    fn reservation_matches_core_single_segment_semantics() {
        for path in [
            "/health",
            "/adm/health",
            "/adm/server/info",
            "/adm/logger",
            "/adm/cache/name",
        ] {
            assert!(reserved_path(path).is_some());
        }
        for path in [
            "/healthz",
            "/health/",
            "/adm/cache",
            "/adm/cache/",
            "/adm/cache/name/",
            "/adm/cache/a/b",
        ] {
            assert!(reserved_path(path).is_none());
        }
    }
    #[tokio::test]
    async fn embedded_security_defaults_are_production_safe() {
        let dir = tempfile::tempdir().unwrap();
        let input = input(Some(dir.path().into()), false).unwrap();
        let prepared = light_lambda_runtime::init::prepare_local(&input)
            .await
            .unwrap();
        let security: serde_json::Value = prepared
            .module_registry
            .load_config(&prepared, "security.yml")
            .unwrap();
        assert!(
            serverless_core::unsupported::validate_security(&security, false).is_ok(),
            "{:?}",
            serverless_core::unsupported::validate_security(&security, false).err()
        );
    }
}
