use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct GatewayIdentity {
    pub function_name: String,
    pub region: String,
    pub account_id: String,
}
#[derive(Clone, Debug)]
pub struct Targets(BTreeMap<String, String>);
impl Targets {
    pub fn new(
        functions: BTreeMap<String, String>,
        identity: &GatewayIdentity,
    ) -> Result<Self, &'static str> {
        if !name(&identity.function_name, 64)
            || identity.account_id.len() != 12
            || !identity.account_id.bytes().all(|b| b.is_ascii_digit())
            || identity.region.is_empty()
        {
            return Err("invalid gateway identity");
        }
        for (alias, target) in &functions {
            if !name(alias, 128) {
                return Err("invalid target alias");
            }
            validate(target, identity)?;
        }
        Ok(Self(functions))
    }
    pub fn get(&self, alias: &str) -> Option<&str> {
        self.0.get(alias).map(String::as_str)
    }
}
fn name(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub fn validate(target: &str, identity: &GatewayIdentity) -> Result<(), &'static str> {
    let parts: Vec<&str> = target.split(':').collect();
    let (function, qualifier) = match parts.as_slice() {
        [function] => (*function, None),
        [function, qualifier] => (*function, Some(*qualifier)),
        [account, "function", function] | [account, "function", function, _] => {
            if *account != identity.account_id {
                return Err("target account mismatch");
            }
            (*function, parts.get(3).copied())
        }
        [
            "arn",
            partition,
            "lambda",
            region,
            account,
            "function",
            function,
        ]
        | [
            "arn",
            partition,
            "lambda",
            region,
            account,
            "function",
            function,
            _,
        ] => {
            if !matches!(*partition, "aws" | "aws-cn" | "aws-us-gov") {
                return Err("invalid ARN partition");
            }
            if *region != identity.region || *account != identity.account_id {
                return Err("target region/account mismatch");
            }
            let expected_partition = if region.starts_with("cn-") {
                "aws-cn"
            } else if region.starts_with("us-gov-") {
                "aws-us-gov"
            } else {
                "aws"
            };
            if *partition != expected_partition {
                return Err("target partition mismatch");
            }
            (*function, parts.get(7).copied())
        }
        _ => return Err("invalid function target"),
    };
    if !name(function, 64) {
        return Err("invalid function name");
    }
    if let Some(q) = qualifier {
        if q != "$LATEST" && !name(q, 128) {
            return Err("invalid function qualifier");
        }
        if q.bytes().all(|b| b.is_ascii_digit())
            && (q.starts_with('0') || q.parse::<u64>().is_err())
        {
            return Err("invalid function version");
        }
    }
    if function == identity.function_name {
        return Err("self invocation forbidden");
    }
    Ok(())
}

/// Explicit process-start test switch, not a request flag. HTTP is restricted to
/// numeric loopback even when enabled; HTTPS always retains SDK TLS validation.
pub fn validate_endpoint(
    endpoint: &str,
    allow_insecure_test_endpoints: bool,
) -> Result<(), &'static str> {
    let url = url::Url::parse(endpoint).map_err(|_| "invalid endpointOverride")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid endpointOverride");
    }
    if url.scheme() == "https" && url.host_str().is_some() {
        return Ok(());
    }
    if allow_insecure_test_endpoints
        && url.scheme() == "http"
        && url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
    {
        return Ok(());
    }
    Err("endpointOverride requires HTTPS or explicitly enabled loopback test endpoint")
}

/// Validated, process-configuration-only override. The boolean must come from
/// LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS=1 at Init (or explicit test wiring).
#[derive(Clone, Debug)]
pub struct EndpointOverride(String);
impl EndpointOverride {
    pub fn new(url: String, allow_insecure_test_endpoints: bool) -> Result<Self, &'static str> {
        validate_endpoint(&url, allow_insecure_test_endpoints)?;
        Ok(Self(url))
    }
    pub(crate) fn into_url(self) -> String {
        self.0
    }
}
