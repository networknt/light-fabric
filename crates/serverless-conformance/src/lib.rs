//! Fixture contracts and incremental application conformance.
mod audit_capture;
mod authorizer;
mod backend_diagnostics;
mod blocking_probe;
mod bootstrap_probe;
mod bounded_work;
mod completion_probe;
mod emission_worker;
pub mod gap_work;
mod gate_component;
mod ldap_dependency;
mod legacy_correlation;
mod logging_component;
mod panic_component;
mod policy_observer;
mod probe_timing;
mod reporting_component;
mod router_failed_component;
mod single_runner;
mod test_config_worker;
mod trace_observer;
mod transformer_observer;
use anyhow::{Context, Result, bail, ensure};
mod approved_output_bounds;
mod blocking_lease;
mod canonical_json;
mod closed_final_faults;
mod emf_route;
mod expectation_consistency;
mod init_logging;
mod init_reporting;
mod jwks_gate_cancellation;
mod oauth_prerequisite;
mod response_policy_probe;
mod test_config_flag;
mod trace_context;
pub use canonical_json::canonical_bytes;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub fn canonical(value: &Value) -> Result<Vec<u8>> {
    fn ordered(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: BTreeMap<_, _> = m.iter().collect();
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(k, v)| (k.clone(), ordered(v)))
                        .collect::<Map<_, _>>(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(ordered).collect()),
            _ => v.clone(),
        }
    }
    Ok(serde_json::to_vec(&ordered(value))?)
}
pub fn fixture_digest(value: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(canonical(value)?)))
}
pub fn file_digest(path: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(canonical_bytes(&fs::read(
        path,
    )?)?)))
}
pub fn fixture_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let files = json_files(dir)?
        .into_iter()
        .filter(|path| {
            !path
                .strip_prefix(dir)
                .expect("collector stays within root")
                .components()
                .next()
                .is_some_and(|part| {
                    part.as_os_str() == "results" || part.as_os_str() == "generated"
                })
        })
        .collect::<Vec<_>>();
    ensure!(!files.is_empty(), "no fixtures in {}", dir.display());
    Ok(files)
}
fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "symlinks are forbidden: {}",
                entry.path().display()
            );
            if kind.is_dir() {
                visit(&entry.path(), out)?;
            } else if entry.path().extension().is_some_and(|e| e == "json") {
                out.push(entry.path());
            }
        }
        Ok(())
    }
    let mut files = vec![];
    visit(dir, &mut files)?;
    files.sort();
    ensure!(!files.is_empty(), "no fixtures in {}", dir.display());
    Ok(files)
}
pub fn load(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path)?).with_context(|| path.display().to_string())
}
pub fn manifest(dir: &Path) -> Result<String> {
    let mut text = String::new();
    for path in fixture_files(dir)? {
        let relative = path
            .strip_prefix(dir)?
            .to_str()
            .context("non UTF-8 path")?
            .replace('\\', "/");
        ensure!(!relative.contains(['\n', '\r']), "invalid fixture path");
        text.push_str(&format!("{}  {}\n", file_digest(&path)?, relative));
    }
    let total = hex::encode(Sha256::digest(text.as_bytes()));
    text.push_str(&format!("{total}  TOTAL\n"));
    Ok(text)
}
pub fn schema_validate(schema: &Value, instance: &Value) -> Result<()> {
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)?;
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    ensure!(errors.is_empty(), "schema errors: {}", errors.join("; "));
    Ok(())
}
pub fn ledger_rows(text: &str) -> Result<BTreeMap<String, String>> {
    let mut rows = BTreeMap::new();
    for line in text.lines().filter(|l| l.starts_with("| <a id=")) {
        let columns: Vec<_> = line.split('|').map(str::trim).collect();
        let id = columns[1]
            .split("</a>")
            .nth(1)
            .context("ledger row id")?
            .trim();
        let status = columns
            .iter()
            .find(|c| matches!(**c, "equivalent" | "intentional-change" | "deferred"))
            .context("ledger status")?;
        ensure!(
            rows.insert(id.to_owned(), status.to_string()).is_none(),
            "duplicate ledger ID {id}"
        );
    }
    ensure!(!rows.is_empty(), "no ledger rows");
    Ok(rows)
}
pub fn cross_validate(f: &Value, ledger: &BTreeMap<String, String>) -> Result<()> {
    expectation_consistency::validate(f)?;
    expectation_consistency::validate_rust_business_maps(f)?;
    jwks_gate_cancellation::validate(f)?;
    approved_output_bounds::validate(f)?;
    if f.get("securityLogProbe").is_some() {
        ensure!(
            f["securityLogProbe"] == "jwt-trace-v1"
                && f["tokens"]
                    .as_object()
                    .is_some_and(|tokens| tokens.len() == 1)
                && f["steps"].as_array().is_some_and(|steps| steps.len() == 1),
            "closed security logging probe requires one signed token and one request"
        );
        ensure!(
            f["config"]["files"]["security"]["logJwtToken"] == false
                && f["ledger"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == "S-14")),
            "security logging observes actual exposure outside token logging flag"
        );
        let step = &f["steps"][0];
        ensure!(
            step["expect"]["securityLogObservation"]
                == serde_json::json!({"fullTokenExposed":false,"tokenFragmentExposed":false,"sensitiveClaimExposed":false}),
            "Rust redacted telemetry must expose no declared synthetic bearer/fragment/secret"
        );
        ensure!(
            step["expectJava"]["securityLogObservation"]["fullTokenExposed"] == true
                && step["expectJava"]["securityLogObservation"]["tokenFragmentExposed"] == true
                && step["expectJava"]["securityLogObservation"]["sensitiveClaimExposed"]
                    == (step["expectJava"]["status"] == 200),
            "actual Java trace exposure must be asserted"
        );
    }
    if f.get("ruleFailureProbe").is_some() {
        ensure!(
            matches!(
                f["ruleFailureProbe"].as_str(),
                Some("caught-response-error-v1" | "caught-request-error-v1")
            ) && f["ruleConfigurationPair"]["rust"]["rule"]["ruleBodies"][if f["ruleFailureProbe"]
                == "caught-request-error-v1"
            {
                "test-edge"
            } else {
                "slow"
            }]["expression"]
                == "int(requestHeaders['test-rule']) > 0",
            "closed response-rule failure recipe required"
        );
        ensure!(
            f["ledger"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == "T-02")),
            "response-rule catch probe requires T-02"
        );
        ensure!(
            f["steps"].as_array().is_some_and(|steps| steps.len() == 1),
            "rule failure probe requires exactly one step"
        );
        let step = &f["steps"][0];
        ensure!(
            step["rawEvent"]["headers"]["test-rule"] == "throw"
                && step["rawEvent"]["multiValueHeaders"]["test-rule"]
                    == serde_json::json!(["throw"]),
            "rule failure requires same explicit single/multi header input"
        );
        for kind in ["expect", "expectJava"] {
            ensure!(
                step[kind]["ruleFailuresCaught"] == 1
                    && step[kind]["status"] == 200
                    && step[kind]["body"] == "ok"
                    && step[kind]["backendCalls"]
                        .as_array()
                        .is_some_and(|a| a.len() == 1),
                "rule catch must preserve reviewed backend response and one dispatch"
            );
        }
    }
    blocking_lease::validate(f)?;
    test_config_flag::validate(f)?;
    trace_context::validate(f)?;
    init_reporting::validate(f)?;
    closed_final_faults::validate(f)?;
    closed_final_faults::validate_ingress_diagnostics(f)?;
    closed_final_faults::validate_configuration_warning(f)?;
    init_logging::validate(f)?;
    oauth_prerequisite::validate(f)?;
    response_policy_probe::validate(f)?;
    for step in f
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![f], |steps| steps.iter().collect())
    {
        for kind in ["expect", "expectJava"] {
            for call in step[kind]["backendCalls"].as_array().into_iter().flatten() {
                if let Some(profile) = call.get("eventCorrelation") {
                    ensure!(
                        f["ledger"]
                            .as_array()
                            .is_some_and(|rows| rows.iter().any(|id| id == "Q-02")),
                        "generated correlation requires Q-02"
                    );
                    let java = kind == "expectJava" || f["javaOnly"] == true;
                    let name = if java {
                        "X-Correlation-Id"
                    } else {
                        "x-correlation-id"
                    };
                    let required = if java {
                        "java-base64url128-v1"
                    } else {
                        "rust-base64url128-v1"
                    };
                    ensure!(
                        profile == required
                            && call["event"]["headers"][name] == "${generated:correlation}"
                            && call["eventExact"]["headers"][name] == "${generated:correlation}",
                        "generated correlation requires closed profile and asserted exact headers"
                    );
                    if !java {
                        ensure!(
                            call["eventExact"]["multiValueHeaders"][name]
                                == serde_json::json!(["${generated:correlation}"]),
                            "generated Rust correlation requires one identical multimap value"
                        );
                    }
                    ensure!(
                        f["nondeterministic"].as_array().is_some_and(Vec::is_empty),
                        "generated correlation profile cannot combine exclusions"
                    );
                }
            }
        }
    }
    if f.get("loggerFilterPrerequisite").is_some() {
        ensure!(
            f["loggerFilterPrerequisite"] == "info-v1"
                && f["ledger"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|id| id == "X-03a")),
            "closed filter prerequisite requires approved X-03a"
        );
        ensure!(
            f["rawEvent"]["path"] == "/adm/logger"
                && f["rawEvent"]["httpMethod"] == "GET"
                && f["expect"]["body"] == "{\"filter\":\"info\"}"
                && f["expect"]["status"] == 200
                && f.get("expectJava").is_some(),
            "closed filter shape and GET input required"
        );
    }
    if let Some(probe) = f.get("boundedWorkProbe") {
        let stages: &[&str] = match probe.as_str() {
            Some("request-100ms-v1" | "request-ingress50-100ms-v1") => &["request"],
            Some("response-100ms-v1") => &["response"],
            Some("request-response-100ms-v1") => &["request", "response"],
            _ => anyhow::bail!("unknown closed bounded work probe"),
        };
        let ingress_limited = probe == "request-ingress50-100ms-v1";
        ensure!(
            !ingress_limited || f["config"]["values"]["light-lambda.ingressTimeoutMs"] == 50,
            "closed ingress work requires its exact50ms hard limit"
        );
        ensure!(
            f["ledger"].as_array().is_some_and(|rows| rows
                .iter()
                .any(|id| matches!(id.as_str(), Some("D-08a" | "D-08b" | "D-08c")))),
            "bounded work requires approved deadline row"
        );
        let handler = &f["config"]["files"]["handler"];
        ensure!(
            handler["paths"].as_array().is_some_and(Vec::is_empty)
                && stages
                    .iter()
                    .all(|stage| handler["chains"][*stage].is_array()
                        && handler["defaultHandlers"]
                            .as_array()
                            .is_some_and(|chain| chain.iter().any(|id| id == *stage))),
            "bounded work cannot bypass its declared stage"
        );
        ensure!(
            f["config"]["files"]
                .get("conformance-bounded-work-probe")
                .is_none()
                && f.get("requestFailureProbe").is_none(),
            "bounded work cannot shadow configuration or combine failure modes"
        );
        for step in f
            .get("steps")
            .and_then(Value::as_array)
            .map_or_else(|| vec![f], |steps| steps.iter().collect())
        {
            ensure!(
                step["context"]["remainingTimeMs"]
                    == if stages.len() == 2 || ingress_limited {
                        150
                    } else {
                        50
                    }
                    && f["config"]["values"]["light-lambda.responseReserveMs"] == 20
                    && f["config"]["values"]["light-lambda.postReserveMs"] == 10,
                "bounded work requires reviewed stage budget with valid20ms response and10ms post reserves"
            );
            for kind in ["expect", "expectJava"] {
                ensure!(
                    step[kind]["boundedWorkProbeExecutions"] == stages.len(),
                    "bounded work must assert execution in both contracts"
                );
            }
        }
    }
    if f.get("ruleInputProbe").is_some() {
        ensure!(
            f["ruleInputProbe"] == "native-response-map-v1",
            "unknown closed rule input probe"
        );
        ensure!(
            f.get("ruleConfigurationPair").is_some(),
            "rule input probe requires approved native/CEL pair"
        );
        for step in f
            .get("steps")
            .and_then(Value::as_array)
            .map_or_else(|| vec![f], |steps| steps.iter().collect())
        {
            let expected = &step["expectJava"]["ruleInputObservation"];
            ensure!(
                expected["executions"] == 1 && expected["input"].is_object(),
                "rule input must assert actual execution and a complete input map"
            );
            ensure!(
                step["expect"]["ruleInputObservation"] == *expected,
                "T-02 preserves the approved response-rule input map in both contracts"
            );
            ensure!(
                expected["input"]["requestHeaders"] == step["rawEvent"]["headers"]
                    && expected["input"]["responseHeaders"] == step["rawEvent"]["headers"],
                "T-02 responseHeaders intentionally reads request headers"
            );
        }
    }
    if f.get("loopbackJwksPrerequisite").is_some() {
        ensure!(
            matches!(
                f["expectInit"]["outcome"].as_str(),
                Some("initSuccess" | "initError")
            ) && f["expectJavaInit"]["outcome"] == "initSuccess",
            "closed JWKS prerequisite requires explicit Init expectations"
        );
        let mut expectations = vec![&f["expectInit"], &f["expectJavaInit"]];
        for step in f
            .get("steps")
            .and_then(Value::as_array)
            .map_or_else(|| vec![f], |steps| steps.iter().collect())
        {
            expectations.extend([&step["expect"], &step["expectJava"]]);
        }
        for expected in expectations {
            ensure!(
                expected["jwksCalls"].as_array().is_some_and(
                    |calls| Some(calls.len() as u64) == expected["jwksFetches"].as_u64()
                ),
                "closed JWKS prerequisite requires exact actual request traces matching count"
            );
        }
        ensure!(
            f["loopbackJwksPrerequisite"] == "synthetic-jks-client-v1",
            "unknown closed JWKS prerequisite"
        );
        ensure!(
            f["config"]["files"]["client"]["tls"]
                == serde_json::json!({
                    "loadKeyStore": false, "loadTrustStore": false, "verifyHostname": true
                }),
            "closed JWKS prerequisite cannot shadow or weaken shared TLS configuration"
        );
        ensure!(
            f["dependencies"].get("jwks").is_some()
                && f["config"]["files"]["security"]["jwt"]["keyResolver"] == "JsonWebKeySet"
                && f["config"]["files"]["security"]["enableVerifyJwt"] == true,
            "closed JWKS prerequisite requires declared JWKS verification"
        );
    }
    if f.get("jwtCachePrerequisite").is_some() {
        ensure!(
            f["config"]["files"].get("service").is_none()
                && f["config"]["files"].get("cache").is_none(),
            "closed JWT caches cannot shadow shared configuration"
        );
        ensure!(
            f["config"]["files"]["security"]["jwt"]["keyResolver"] == "JsonWebKeySet"
                && f["dependencies"].get("jwks").is_some(),
            "JWT cache prerequisite requires declared JWKS verification"
        );
    }
    for step in f
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![f], |steps| steps.iter().collect())
    {
        if step["expect"].get("emf").is_some() {
            ensure!(
                f["config"]["files"]["handler"]["paths"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "closed EMF profile cannot bypass its default metrics chain"
            );
            ensure!(
                f["config"]["files"]["handler"]["chains"]["request"]
                    .as_array()
                    .is_some_and(|handlers| handlers.iter().any(|name| name == "metrics")),
                "closed EMF profile requires request metrics alias"
            );
            ensure!(
                step["context"]["functionName"] == step["expect"]["emf"]["functionName"],
                "EMF function dimension requires explicit invocation context"
            );
            emf_route::validate(f, step)?;
            ensure!(
                f["ledger"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == "M-01")),
                "closed EMF expectation requires M-01"
            );
            ensure!(
                step["expectJava"].get("emf").is_none() && step.get("expectJava").is_some(),
                "EMF requires separate Java expectation"
            );
            ensure!(
                f["config"]["files"]["metrics"] == serde_json::json!({"enabled":true}),
                "EMF alias must not select a deferred network sink"
            );
            let error = if step["expect"]["status"]
                .as_u64()
                .context("EMF response status")?
                >= 400
            {
                1
            } else {
                0
            };
            ensure!(
                step["expect"]["emf"]["errors"] == error,
                "EMF Errors must match response outcome"
            );
        }
    }
    if f.get("runtimeIdentity").is_some() {
        ensure!(
            f["ledger"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == "D-07")),
            "closed process-start function identity requires D-07"
        );
    }
    if f.get("ruleConfigurationPair").is_some() {
        let ids = f["ledger"].as_array().context("ledger must be an array")?;
        ensure!(
            ids.iter().any(|id| id == "T-01") && ids.iter().any(|id| id == "T-02"),
            "closed rule configuration pair requires T-01 and T-02"
        );
        ensure!(
            f["config"]["files"].get("rule").is_none()
                && f["config"]["files"].get("service").is_none(),
            "closed paired rule configuration cannot override shared rule/service configuration"
        );
        ensure!(
            f["javaOnly"] == false && f["rustOnly"] == false && f.get("expectRustInit").is_none(),
            "paired rule configuration requires both runners to reach transformation"
        );
    }
    if f["dependencies"].get("ldap").is_some() {
        ensure!(
            f["config"]["files"]["ldap"]["uri"] == "${dependency:ldap}"
                && f["ledger"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == "S-16")),
            "LDAP dependency is closed to S-16 loopback bind"
        );
    }
    if f["component"] == "javaBootstrapP01a" {
        ensure!(
            f["javaOnly"] == true
                && f["ledger"] == serde_json::json!(["P-01a"])
                && f["expect"]["outcome"] == "bootstrap",
            "bootstrap component is closed to Java-only P-01a"
        );
    }
    if f.get("dynamoProbe").is_some() {
        let paired_rejection = f["id"] == "K-01-selected-dynamo-deferred-init-contract-v1"
            && f["javaOnly"] == false
            && f["rustOnly"] == false
            && f["ledger"] == serde_json::json!(["K-01", "CFG-04"])
            && f["dynamoProbe"] == "crud"
            && f["config"]["files"]["service"]
                == serde_json::json!({"singletons":[{"com.networknt.cache.CacheManager":["com.networknt.aws.lambda.cache.DynamoDbCacheManager"]}]})
            && f["expect"]
                == serde_json::json!({"outcome":"initError","code":"InvalidConfiguration","backendCalls":[],"jwksFetches":0})
            && f["expectJava"].get("dynamoObservation").is_some();
        ensure!(
            f["initOnly"] == true
                && (paired_rejection
                    || (f["javaOnly"] == true
                        && f["ledger"] == serde_json::json!(["K-01"])
                        && f["expect"].get("dynamoObservation").is_some())),
            "Dynamo probe is Java-only execution, or the closed paired deferred-manager Init rejection"
        );
    }
    if f.get("configurationProbe").is_some() {
        ensure!(
            f["initOnly"] == true && f["ledger"] == serde_json::json!(["CFG-03"]),
            "configuration probe is closed to CFG-03 Init"
        );
        ensure!(
            f["expect"].get("configurationObservation").is_some(),
            "decryption observation must be asserted"
        );
    }
    if f["dependencies"].get("invoke").is_some() {
        ensure!(
            f["config"]["files"]["lambda-proxy"]["endpointOverride"] == "${dependency:invoke}"
                && f["config"]["files"]["lambda-proxy"]["region"] == "us-east-1",
            "Invoke dependency requires explicit synthetic endpoint and region"
        );
        ensure!(
            f["dependencies"].get("http").is_none(),
            "Invoke and HTTP dependencies must be isolated"
        );
    }
    if f["component"] == "javaStreamC07" {
        ensure!(
            f["javaOnly"] == true
                && f["rustOnly"] == false
                && f["ledger"] == serde_json::json!(["C-07"]),
            "stream component is closed to Java-only C-07"
        );
        ensure!(
            matches!(
                f["expect"]["outcome"].as_str(),
                Some("streamOutput" | "exception")
            ),
            "stream component requires stream output or Init exception"
        );
        if f["expect"]["outcome"] == "exception" {
            ensure!(
                f["expect"]["phase"] == "init",
                "stream exception must be Init"
            );
        }
    } else {
        let cases: Vec<&Value> = f["steps"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_else(|| vec![f]);
        for case in cases {
            ensure!(
                case["expect"]["outcome"] != "streamOutput"
                    && case["expectJava"]["outcome"] != "streamOutput",
                "stream output cannot stand in for ALB response"
            );
        }
    }
    if let Some(replies) = f["dependencies"]["http"]["responses"].as_array() {
        for reply in replies
            .iter()
            .filter(|reply| reply.get("relativeIntrospectionExpSec").is_some())
        {
            ensure!(
                f["ledger"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == "S-15")),
                "relative introspection expiry requires S-15"
            );
            let body: Value =
                serde_json::from_str(reply["body"].as_str().context("introspection body")?)?;
            let object = body.as_object().context("introspection response object")?;
            ensure!(
                object.len() == 4
                    && object.get("exp") == Some(&Value::Null)
                    && body["active"].is_boolean()
                    && body["client_id"].is_string()
                    && body["iss"].is_string(),
                "relative introspection recipe requires exactly active/client_id/iss/null exp"
            );
        }
    }
    let single = f["javaOnly"] == true || f["rustOnly"] == true;
    let difference = f.get("differences").is_some();
    let ids = f["ledger"].as_array().context("ledger array")?;
    let mut intentional = false;
    for id in ids {
        let id = id.as_str().context("ledger id")?;
        let status = ledger
            .get(id)
            .with_context(|| format!("unknown ledger ID {id}"))?;
        intentional |= status == "intentional-change";
        ensure!(
            !single || status != "equivalent",
            "single-runner maps equivalent row {id}"
        );
    }
    ensure!(
        !difference || intentional,
        "differences require approved intentional-change row"
    );
    let steps: Vec<&Value> = match f.get("steps") {
        Some(s) => s.as_array().context("steps")?.iter().collect(),
        None => vec![f],
    };
    for (step_index, step) in steps.iter().enumerate() {
        if let Some(report) = step.get("metricsReport") {
            ensure!(
                ids.iter().any(|id| id == "M-02")
                    && f["config"]["files"]["metrics"]["serverHost"] == "127.0.0.1"
                    && f["config"]["files"]["metrics"]["serverPort"] == "${dependency:http:port}",
                "APM sink must use declared owned loopback"
            );
            ensure!(
                f["config"]["files"]["metrics"]["reportInMinutes"]
                    == if report["mode"] == "scheduled" { 1 } else { 60 },
                "APM schedule must match the closed declaration"
            );
        }
        for expectation in [step.get("expect"), step.get("expectJava")]
            .into_iter()
            .flatten()
        {
            for call in expectation["httpCalls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|call| call.get("apmMetrics").is_some())
            {
                ensure!(
                    step.get("metricsReport").is_some()
                        && ids.iter().any(|id| id == "M-02")
                        && call["uri"] == "/metrics",
                    "APM body assertion is exclusive to M-02 sink"
                );
            }
        }
        if step.get("concurrentCopies").is_some() {
            ensure!(
                (ids.iter().any(|id| id == "S-13b")
                    || f["blockingLeasePrerequisite"] == "response-300ms-capacity1-two-callers-v1")
                    && step["concurrentCopies"] == 2,
                "concurrency representation is closed to two S-13b callers"
            );
            ensure!(
                step["expectJava"]["outcome"] == "parallelResponses"
                    && (f["blockingLeasePrerequisite"]
                        == "response-300ms-capacity1-two-callers-v1"
                        || step["backend"].as_array().is_some_and(Vec::is_empty)),
                "parallel Java contrast requires explicit responses and no backend calls"
            );
        }
        if step.get("warmConfig").is_some() {
            ensure!(
                step_index > 0 && ids.iter().any(|id| id == "CFG-05"),
                "warm config requires CFG-05 after initialization"
            );
        }
        for expected in [step.get("expect"), step.get("expectJava")]
            .into_iter()
            .flatten()
        {
            if let Some(timing) = expected.get("invocationTiming") {
                ensure!(
                    timing["minimumMs"].as_u64() <= timing["maximumMs"].as_u64(),
                    "invalid invocation timing bounds"
                );
            }
        }
        let status_probe = step["observeStatusSerialization"] == true;
        let own_expectation = step.get("expectJava").unwrap_or(&step["expect"]);
        ensure!(
            status_probe == own_expectation.get("statusSerialization").is_some(),
            "status component requires explicit observation and expectation"
        );
        if status_probe {
            ensure!(
                ids.iter().any(|id| id == "E-01"),
                "status component requires E-01"
            );
            ensure!(
                f.get("expectRustInit").is_some(),
                "test status plugin requires Rust Init non-execution"
            );
            ensure!(f["config"]["files"]["handler"]["handlers"].as_array().is_some_and(|handlers| handlers.iter().any(|h| h.as_str().is_some_and(|h| h.starts_with("com.networknt.aws.lambda.conformance.ConformanceStatusMetadataMiddleware@")))), "unreviewed status probe binding");
        }
        let observes_audit = step["observeAudit"] == true;
        let java_expected = step.get("expectJava").unwrap_or(&step["expect"]);
        let failure_probe = f.get("requestFailureProbe");
        ensure!(
            failure_probe.is_some() == java_expected.get("failureProbeResult").is_some()
                && failure_probe.is_some() == step["expect"].get("failureProbeResult").is_some(),
            "closed failure probe requires both explicit expectations"
        );
        if let Some(mode) = failure_probe {
            ensure!(
                f["config"]["files"]["handler"]["paths"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "closed failure probe cannot be bypassed by explicit path routes"
            );
            ensure!(
                ids.iter().any(|id| id == "C-04")
                    && f["javaOnly"] == false
                    && f["rustOnly"] == false,
                "closed failure probe requires shared C-04 execution"
            );
            ensure!(
                f["dependencies"].get("invoke").is_none() && f.get("initOnly").is_none(),
                "closed failure probe uses fake dispatch with request-bearing input"
            );
            ensure!(
                f["config"]["files"]
                    .get("conformance-failure-probe")
                    .is_none(),
                "failure probe configuration is runner-owned"
            );
            ensure!(
                f["config"]["files"]["handler"]["chains"]["request"].is_array()
                    && f["config"]["files"]["handler"]["defaultHandlers"]
                        .as_array()
                        .is_some_and(|chain| chain.iter().any(|handler| handler == "request")),
                "closed failure probe needs executed request chain"
            );
            ensure!(
                &java_expected["failureProbeResult"]["mode"] == mode
                    && &step["expect"]["failureProbeResult"]["mode"] == mode,
                "failure probe mode must match both assertions"
            );
        }
        for expected in [java_expected, &step["expect"]] {
            for call in expected["backendCalls"].as_array().into_iter().flatten() {
                if let Some(authorizer) = call.get("eventAuthorizer") {
                    ensure!(
                        ids.iter().any(|id| id == "E-08"),
                        "verified authorizer assertion requires E-08"
                    );
                    let marker = authorizer["claimsFromToken"]
                        .as_str()
                        .context("authorizer input token reference required")?;
                    let name = marker
                        .strip_prefix("${token:")
                        .and_then(|v| v.strip_suffix('}'))
                        .context("authorizer must reference a declared input token")?;
                    let claims = &f["tokens"][name]["claims"];
                    ensure!(
                        claims.is_object(),
                        "authorizer input token must be declared"
                    );
                    let principal = ["sub", "cid", "client_id", "user_id"]
                        .iter()
                        .find_map(|key| claims.get(key));
                    ensure!(
                        principal == authorizer.get("principalId"),
                        "authorizer principal must match approved input-claim precedence"
                    );
                    let scope_claim = claims.get("scope").or_else(|| claims.get("scp"));
                    let scope = match scope_claim {
                        None => None,
                        Some(Value::String(value)) => {
                            Some(value.split_whitespace().collect::<Vec<_>>().join(" "))
                        }
                        Some(Value::Array(values)) => Some(
                            values
                                .iter()
                                .map(|v| {
                                    v.as_str()
                                        .context("authorizer scope list must contain strings")
                                })
                                .collect::<Result<Vec<_>>>()?
                                .join(" "),
                        ),
                        Some(_) => bail!("authorizer scope claim type is unsupported"),
                    };
                    ensure!(
                        scope.as_deref() == authorizer.get("scope").and_then(Value::as_str),
                        "authorizer scope must match declared input claims"
                    );
                    let bearer = format!("Bearer {marker}");
                    ensure!(
                        step["rawEvent"]["multiValueHeaders"]["authorization"]
                            .as_array()
                            .is_some_and(|values| values
                                .iter()
                                .any(|value| value.as_str() == Some(&bearer))),
                        "closed authorizer marker requires the same declared ALB multimap input token"
                    );
                    ensure!(
                        call["event"]["requestContext"].get("authorizer").is_none(),
                        "authorizer exact marker cannot overlap a partial authorizer expectation"
                    );
                }
            }
            if expected["backendCalls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call.get("eventClock").is_some()))
            {
                ensure!(
                    ids.iter().any(|id| id == "E-10g"),
                    "gateway request clock constraints require E-10g"
                );
                ensure!(
                    f["clock"]["mode"] == "relative-wall",
                    "gateway clock constraints require explicit relative wall-clock handling"
                );
            }
        }
        let completion_probe = step.get("completionProbe").is_some();
        ensure!(
            completion_probe == java_expected.get("completionProbeResult").is_some()
                && completion_probe == step["expect"].get("completionProbeResult").is_some(),
            "closed completion probe requires both explicit expectations"
        );
        if completion_probe {
            ensure!(
                ids.iter().any(|id| id == "C-03"),
                "completion probe requires C-03"
            );
            ensure!(
                f["dependencies"].get("invoke").is_none(),
                "completion probe uses the fake dispatch adapter only"
            );
        }
        let diagnostics = step["observeBackendDiagnostics"] == true;
        ensure!(
            diagnostics == java_expected.get("backendDiagnostics").is_some()
                && diagnostics == step["expect"].get("backendDiagnostics").is_some(),
            "backend diagnostics require a declaration and both expectations"
        );
        if diagnostics {
            ensure!(
                ids.iter().any(|id| id == "D-03"),
                "backend diagnostics require D-03"
            );
            ensure!(
                java_expected["backendDiagnostics"]["kind"] == "javaFunctionErrorTrace"
                    && step["expect"]["backendDiagnostics"]["kind"]
                        == "rustFunctionErrorSafeMetadata",
                "backend diagnostics require the closed Java trace/Rust safe metadata pair"
            );
            ensure!(
                step["backend"]
                    .as_array()
                    .is_some_and(|calls| calls.len() == 1
                        && calls[0]["match"]["target"] == "FnX"
                        && matches!(
                            calls[0]["respond"]["functionError"].as_str(),
                            Some("Handled" | "Unhandled")
                        )),
                "backend diagnostics require one declared FnX FunctionError reply"
            );
        }
        ensure!(
            observes_audit == java_expected.get("auditRecords").is_some(),
            "audit records require an explicit audit sink declaration and expectation"
        );
        if observes_audit {
            ensure!(
                ids.iter().any(|id| id == "A-01" || id == "A-02"),
                "audit sink requires A-01/A-02"
            );
            ensure!(
                f["config"]["files"]["audit"]["timestampFormat"] == "'synthetic-time'",
                "audit sink uses the explicitly reviewed literal timestamp format"
            );
        }
        if let Some(actions) = step["cacheActions"].as_array() {
            ensure!(
                f.get("expectRustInit").is_some(),
                "synthetic Java cache actions require explicit Rust Init non-execution"
            );
            let effects = step["expectJava"]["cacheActions"]
                .as_array()
                .context("cache actions require declared effects")?;
            ensure!(
                effects.len() == actions.len(),
                "cache effect count mismatch"
            );
            for (action, effect) in actions.iter().zip(effects) {
                ensure!(
                    action
                        .as_object()
                        .context("cache action")?
                        .iter()
                        .all(|(k, v)| effect.get(k) == Some(v)),
                    "cache effect differs from declaration"
                );
                if action["kind"] == "jwtCachedNbf" {
                    ensure!(
                        ids.iter().any(|id| id == "S-17" || id == "S-18"),
                        "cached nbf action requires S-17/S-18"
                    );
                    let recipe = action["tokenRecipe"]
                        .as_str()
                        .context("cache token recipe")?;
                    let token = f["tokens"]
                        .get(recipe)
                        .context("cache mutation token missing")?;
                    ensure!(
                        token["signAtStep"].as_u64().unwrap_or(0) <= step_index as u64,
                        "cache token used before signing"
                    );
                } else {
                    ensure!(
                        ids.iter().any(|id| id == "X-05"),
                        "synthetic cache seed requires X-05"
                    );
                }
            }
        }
        ensure!(
            (step["expect"]["outcome"] == "notExecuted") == f.get("expectRustInit").is_some(),
            "notExecuted requires the one declared Rust Init rejection"
        );
        ensure!(
            step["expectJava"]["outcome"] != "notExecuted",
            "Java invocation cannot be skipped inside a warm fixture"
        );
        if let Some(stubs) = step.get("backend").and_then(Value::as_array) {
            let mut required = 0;
            for field in ["expect", "expectJava"] {
                if let Some(calls) = step[field]["backendCalls"].as_array() {
                    required = required.max(calls.len());
                    for (index, call) in calls.iter().enumerate() {
                        ensure!(
                            stubs.get(index).is_some_and(|stub| stub["match"]["target"]
                                == call["target"]
                                || stub["match"]["targets"]
                                    .as_array()
                                    .is_some_and(|targets| targets.contains(&call["target"]))),
                            "{field} backend target/order differs from shared stub at {index}"
                        );
                    }
                }
            }
            ensure!(
                stubs.len() == required,
                "unused or missing shared backend stubs"
            );
            for (index, stub) in stubs.iter().enumerate() {
                if let Some(targets) = stub["match"]["targets"].as_array() {
                    let java = step["expectJava"]["backendCalls"]
                        .get(index)
                        .and_then(|c| c["target"].as_str());
                    let rust = step["expect"]["backendCalls"]
                        .get(index)
                        .and_then(|c| c["target"].as_str());
                    ensure!(
                        difference
                            && f["javaOnly"] == false
                            && f["rustOnly"] == false
                            && java.is_some()
                            && rust.is_some()
                            && java != rust
                            && targets.len() == 2
                            && targets.contains(&serde_json::json!(java.unwrap()))
                            && targets.contains(&serde_json::json!(rust.unwrap())),
                        "target alternatives must be exactly the two distinct declared runner targets at this call index"
                    );
                }
            }
        }
        for (runner, expected) in [("rust", &step["expect"]), ("java", &step["expectJava"])] {
            if expected.get("bodyJson").is_some() {
                ensure!(
                    ids.iter().any(|id| id == "X-02" || id == "X-02a"),
                    "structured body expectation requires X-02/X-02a"
                );
                ensure!(
                    runner == "java" || f["javaOnly"] == true,
                    "Java heap constraints cannot define a Rust ServerInfo shape"
                );
            }
        }
        ensure!(
            step.get("expectJava").is_some() == difference,
            "expectJava/differences mismatch"
        );
        if let Some(java) = step.get("expectJava") {
            let rust = &step["expect"];
            let declared: BTreeSet<_> = f["differences"]
                .as_array()
                .context("differences")?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for field in [
                "outcome",
                "status",
                "body",
                "bodyBase64",
                "backendCalls",
                "jwksFetches",
                "httpCalls",
                "cacheActions",
                "completions",
                "completionProbeResult",
                "finalizationProbe",
                "auditRecords",
                "backendDiagnostics",
                "responsePolicyObservation",
                "statusSerialization",
                "responses",
                "ldapBinds",
                "invocationTiming",
                "securityLogObservation",
            ] {
                ensure!(
                    java.get(field) == rust.get(field) || declared.contains(field),
                    "undeclared difference {field}"
                );
            }
            ensure!(
                java.get("bodyJson") == rust.get("bodyJson") || declared.contains("body"),
                "structured body difference must declare body"
            );
            if java.get("headers") != rust.get("headers")
                || java.get("responseMaps") != rust.get("responseMaps")
            {
                let mut names = BTreeSet::new();
                for expected in [java, rust] {
                    if let Some(headers) = expected["headers"].as_array() {
                        for pair in headers {
                            names.insert(
                                pair[0]
                                    .as_str()
                                    .context("header name")?
                                    .to_ascii_lowercase(),
                            );
                        }
                    }
                }
                for name in names {
                    // Match incremental comparison's case-insensitive header-name
                    // contract. Preserve raw expectations; values, multiplicity
                    // and per-name order still require exact declarations.
                    let values = |v: &Value| -> Vec<Value> {
                        v["headers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|p| {
                                p[0].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&name))
                            })
                            .map(|p| p[1].clone())
                            .collect()
                    };
                    ensure!(
                        values(java) == values(rust)
                            || declared.contains(format!("header:{name}").as_str()),
                        "undeclared header difference {name}"
                    );
                }
            }
        }
    }
    if let Some(http) = f["dependencies"].get("http") {
        let stubs = http["responses"].as_array().context("HTTP responses")?;
        let mut maximum = 0;
        for field in ["expect", "expectJava"] {
            if field == "expectJava" && !difference {
                continue;
            }
            let mut index = 0;
            for step in &steps {
                let calls = step[field]["httpCalls"].as_array().context(
                    "HTTP dependency needs explicit call expectations for every runner/step",
                )?;
                for call in calls {
                    let stub = stubs.get(index).context("missing HTTP response stub")?;
                    ensure!(
                        call["method"] == stub["method"] && call["uri"] == stub["uri"],
                        "HTTP stub method/URI order mismatch"
                    );
                    ensure!(
                        call["target"]
                            == format!(
                                "${{dependency:http}}{}",
                                call["uri"].as_str().context("HTTP URI")?
                            ),
                        "HTTP target must identify the declared loopback dependency and exact URI"
                    );
                    index += 1;
                }
            }
            maximum = maximum.max(index);
        }
        ensure!(stubs.len() == maximum, "unused HTTP response stubs");
    }
    for (name, token) in f["tokens"].as_object().context("tokens")? {
        if token.get("signingKind").is_some() {
            ensure!(
                token["signingKind"] == "hmac"
                    && token["alg"] == "HS256"
                    && ids.iter().any(|id| id == "S-03"),
                "HMAC rejection recipe is closed to S-03 HS256"
            );
        } else {
            ensure!(
                token["alg"] != "HS256",
                "HS256 requires explicit generated HMAC signing"
            );
        }
        let kid = token["kid"].as_str().context("kid")?;
        ensure!(f["keys"].get(kid).is_some(), "token key {kid} missing");
        if let Some(signer) = token.get("signingKey").and_then(Value::as_str) {
            ensure!(
                f["keys"].get(signer).is_some(),
                "token signing key {signer} missing"
            );
        }
        if let Some(at) = token.get("signAtStep") {
            let at = at.as_u64().context("signAtStep")? as usize;
            ensure!(
                at < steps.len() && f.get("initOnly").is_none(),
                "token signing step outside invocation steps"
            );
            for step in steps.iter().take(at) {
                let input = step
                    .get("rawEvent")
                    .or_else(|| step.get("request"))
                    .context("input")?;
                ensure!(
                    !input.to_string().contains(&format!("${{token:{name}}}")),
                    "token {name} used before its signing step"
                );
            }
        }
        let claims = token["claims"].as_object().context("claims")?;
        ensure!(
            !["iat", "exp", "nbf"]
                .iter()
                .any(|k| claims.contains_key(*k)),
            "use relative time recipe, not absolute token time"
        );
    }
    for ids in std::iter::once(&f["dependencies"]["jwks"]["publishedKeys"])
        .chain(steps.iter().map(|step| &step["jwksPublishedKeys"]))
        .chain(
            f["dependencies"]["jwks"]["routes"]
                .as_object()
                .into_iter()
                .flat_map(|routes| routes.values()),
        )
    {
        if let Some(ids) = ids.as_array() {
            for id in ids {
                let id = id.as_str().context("published key")?;
                ensure!(
                    f["keys"].get(id).is_some(),
                    "published JWKS key {id} missing"
                );
            }
        }
    }
    ensure!(
        f["dependencies"]["jwks"]["routes"].get("/jwks").is_none(),
        "JWKS default route cannot be redefined"
    );
    Ok(())
}
pub fn validate(root: &Path, dir: &Path, results: Option<&Path>) -> Result<usize> {
    let schema = load(&root.join("schema/fixture-v1.schema.json"))?;
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md"))?)?;
    let mut fixtures = BTreeMap::new();
    let mut digests = BTreeMap::new();
    for path in fixture_files(dir)? {
        let fixture = load(&path)?;
        schema_validate(&schema, &fixture).with_context(|| path.display().to_string())?;
        cross_validate(&fixture, &ledger).with_context(|| path.display().to_string())?;
        let id = fixture["id"].as_str().context("id")?.to_string();
        digests.insert(id.clone(), file_digest(&path)?);
        ensure!(
            path.file_stem().and_then(|s| s.to_str()) == Some(id.as_str()),
            "filename differs from ID {id}"
        );
        ensure!(
            fixtures.insert(id.clone(), fixture).is_none(),
            "duplicate fixture {id}"
        );
    }
    if let Some(results) = results {
        let schema = load(&root.join("schema/result-v1.schema.json"))?;
        let mut seen = BTreeSet::new();
        for path in json_files(results)? {
            let result = load(&path)?;
            schema_validate(&schema, &result)?;
            let id = result["id"].as_str().context("result id")?;
            let f = fixtures
                .get(id)
                .with_context(|| format!("unknown result fixture {id}"))?;
            ensure!(seen.insert(id.to_owned()), "duplicate result {id}");
            ensure!(result["fixtureDigest"] == digests[id], "stale digest {id}");
            let skip = if result["runner"] == "java" {
                f["rustOnly"] == true
            } else {
                f["javaOnly"] == true
            };
            ensure!(
                (result["execution"] == "skipped") == skip,
                "skip-kind mismatch {id}"
            );
            if result["execution"] == "passed" {
                if f.get("expectRustInit").is_some() {
                    let rust = result["runner"] == "rust";
                    ensure!(
                        result["initialization"]["outcome"]
                            == if rust { "initError" } else { "initSuccess" },
                        "warm Init representation mismatch {id}"
                    );
                    for observation in result["observations"].as_array().context("observations")? {
                        ensure!(
                            (observation["outcome"] == "notExecuted") == rust,
                            "warm invocation/Init skip mismatch {id}"
                        );
                    }
                } else {
                    ensure!(
                        result["observations"]
                            .as_array()
                            .context("observations")?
                            .iter()
                            .all(|v| v["outcome"] != "notExecuted"),
                        "undeclared Init step skips {id}"
                    );
                }
                let count = f.get("steps").and_then(Value::as_array).map_or(1, Vec::len);
                ensure!(
                    result["observations"]
                        .as_array()
                        .context("observations")?
                        .len()
                        == count,
                    "step count {id}"
                );
            }
        }
        ensure!(
            seen.len() == fixtures.len(),
            "missing results: {} of {}",
            seen.len(),
            fixtures.len()
        );
    }
    Ok(fixtures.len())
}
pub mod amendments;
mod application;
pub mod declarations;
pub mod incremental;
mod recipes;
mod tokens;

pub fn command(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|s| s == "__logging-component") {
        return logging_component::command(args);
    }
    if args.first().is_some_and(|s| s == "__emission-worker") {
        return emission_worker::command(args);
    }
    if args.first().is_some_and(|s| s == "__test-config-worker") {
        return test_config_worker::command(args);
    }
    if matches!(args.first().map(String::as_str), Some("run" | "compare")) {
        return incremental::command(args);
    }
    let action = args
        .first()
        .context("usage: digest|validate [fixtures-dir] [--results results-dir]")?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let dir = args
        .get(1)
        .map_or_else(|| root.join("fixtures/v1"), PathBuf::from);
    match action.as_str() {
        "digest" => {
            ensure!(args.len() <= 2, "digest arguments");
            let text = manifest(&dir)?;
            fs::write(dir.join("DIGEST"), &text)?;
            print!("{text}");
        }
        "validate" => {
            let results = if args.len() == 4 && args[2] == "--results" {
                Some(Path::new(&args[3]))
            } else {
                ensure!(args.len() <= 2, "validate arguments");
                None
            };
            println!(
                "validated {} fixtures{}",
                validate(&root, &dir, results)?,
                if results.is_some() {
                    " and results"
                } else {
                    ""
                }
            );
        }
        _ => bail!("only digest and validate are available"),
    }
    Ok(())
}
