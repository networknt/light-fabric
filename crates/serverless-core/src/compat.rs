//! Fixed class identities, independent of deployment-specific aliases. Availability
//! is explicit: a supported future policy still requires an installed factory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HandlerId {
    Metrics,
    Limit,
    Traceability,
    Correlation,
    RequestCors,
    ResponseCors,
    RequestHeader,
    ResponseHeader,
    RequestTransformer,
    ResponseTransformer,
    Audit,
    Token,
    Router,
    Specification,
    Security,
    Jwt,
    Swt,
    Basic,
    ApiKey,
    Sanitizer,
    Validator,
    Proxy,
    Health,
    Info,
    GetLogger,
    SetLogger,
    Cache,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Equivalent,
    IntentionalChange,
    Deferred,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Request,
    Response,
    Dispatch,
    Local,
}
#[derive(Clone, Copy, Debug)]
pub struct Compatibility {
    pub class: &'static str,
    pub alias: &'static str,
    pub id: HandlerId,
    pub phase: Phase,
    pub disposition: Disposition,
}
macro_rules! entry {
    ($class:literal, $alias:literal, $id:ident, $phase:ident, $status:ident) => {
        Compatibility {
            class: concat!("com.networknt.aws.lambda.handler.", $class),
            alias: $alias,
            id: HandlerId::$id,
            phase: Phase::$phase,
            disposition: Disposition::$status,
        }
    };
}
pub const COMPATIBILITY: &[Compatibility] = &[
    entry!(
        "middleware.metrics.APMMetricsMiddleware",
        "metrics",
        Metrics,
        Request,
        IntentionalChange
    ),
    entry!(
        "middleware.metrics.InfluxMetricsMiddleware",
        "metrics",
        Metrics,
        Request,
        Deferred
    ),
    entry!(
        "middleware.limit.LimitMiddleware",
        "limit",
        Limit,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.traceability.TraceabilityMiddleware",
        "traceability",
        Traceability,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.correlation.CorrelationMiddleware",
        "correlation",
        Correlation,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.cors.RequestCorsMiddleware",
        "requestCors",
        RequestCors,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.cors.ResponseCorsMiddleware",
        "responseCors",
        ResponseCors,
        Response,
        Equivalent
    ),
    entry!(
        "middleware.header.RequestHeaderMiddleware",
        "requestHeader",
        RequestHeader,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.header.ResponseHeaderMiddleware",
        "responseHeader",
        ResponseHeader,
        Response,
        Equivalent
    ),
    entry!(
        "middleware.transformer.RequestTransformerMiddleware",
        "requestTransformer",
        RequestTransformer,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.transformer.ResponseTransformerMiddleware",
        "responseTransformer",
        ResponseTransformer,
        Response,
        Equivalent
    ),
    entry!(
        "middleware.audit.AuditMiddleware",
        "audit",
        Audit,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.token.TokenMiddleware",
        "token",
        Token,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.router.LambdaRouterMiddleware",
        "router",
        Router,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.specification.OpenApiMiddleware",
        "specification",
        Specification,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.security.UnifiedSecurityMiddleware",
        "security",
        Security,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.security.JwtVerifyMiddleware",
        "jwt",
        Jwt,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.security.SwtVerifyMiddleware",
        "swt",
        Swt,
        Request,
        Deferred
    ),
    entry!(
        "middleware.security.BasicAuthMiddleware",
        "basic",
        Basic,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.security.ApiKeyMiddleware",
        "apikey",
        ApiKey,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.sanitizer.SanitizerMiddleware",
        "sanitizer",
        Sanitizer,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.validator.ValidatorMiddleware",
        "validator",
        Validator,
        Request,
        Equivalent
    ),
    entry!(
        "middleware.proxy.LambdaProxyMiddleware",
        "proxy",
        Proxy,
        Dispatch,
        Equivalent
    ),
    entry!(
        "health.HealthCheckHandler",
        "health",
        Health,
        Local,
        Equivalent
    ),
    entry!("info.ServerInfoHandler", "info", Info, Local, Equivalent),
    entry!(
        "logger.LoggerGetHandler",
        "getLogger",
        GetLogger,
        Local,
        Equivalent
    ),
    entry!(
        "logger.LoggerSetHandler",
        "setLogger",
        SetLogger,
        Local,
        IntentionalChange
    ),
    entry!(
        "cache.CacheExplorerHandler",
        "cache",
        Cache,
        Local,
        Deferred
    ),
];
pub fn lookup(class: &str) -> Option<&'static Compatibility> {
    COMPATIBILITY.iter().find(|c| c.class == class)
}
