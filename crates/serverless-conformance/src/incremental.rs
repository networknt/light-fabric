//! Incremental, fail-closed adapters. Coverage is deliberately NOT result-v1.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

// Admission is intentionally explicit. Adding an adapter requires a regression
// against its complete expectation vocabulary; a registered handler alone is not
// proof that a fixture's probes, dependencies or assertions are implemented.
pub const SUPPORTED: &[&str] = &[
    "E-03-response-off-0-multi-v1",
    "E-03-response-off-1-multi-v1",
    "E-03-response-off-2-multi-v1",
    "E-03-response-off-3-multi-v1",
    "Q-02-empty-correlation-multi-contract-v1",
    "Q-02-repeated-correlation-multi-contract-v1",
    "B-04-Q-02-header-prefix-correlation-multi-contract-v1",
    "B-04-remove-update-prefix-collision-multi-contract-v1",
    "S-07-valid-multi-policy-0-v1",
    "S-07-valid-multi-policy-1-v1",
    "S-08-missing-basic-multi-contract-v1",
    "S-09-anonymous-multi-policy-v1",
    "S-10-bearer-gate-multi-policy-v1",
    "S-11-valid-multi-policy-v1",
    "S-11-unmatched-multi-policy-v1",
    "V-03-json-object-multi-contract-v1",
    "V-03-html-content-encoder-multi-contract-v1",
    "V-03-html-attribute-golden-encoder-multi-contract-v1",
    "V-03-javascript-golden-encoder-multi-contract-v1",
    "V-03-css-string-golden-encoder-multi-contract-v1",
    "V-03-uri-component-golden-encoder-multi-contract-v1",
    "V-03-xml-content-encoder-multi-contract-v1",
    "V-03-malformed-json-multi-contract-v1",
    "V-03-get-body-bypass-multi-contract-v1",
];

#[derive(Debug)]
pub struct DifferentialMismatch {
    pub observed: BTreeSet<String>,
    pub declared: BTreeSet<String>,
    pub unexpected: BTreeSet<String>,
    pub stale: BTreeSet<String>,
}
impl std::fmt::Display for DifferentialMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "undeclared or stale differences: observed {:?}, declared {:?}; unexpected {:?}; stale {:?}",
            self.observed, self.declared, self.unexpected, self.stale
        )
    }
}
impl std::error::Error for DifferentialMismatch {}

/// Two independent axes: a missing fixture adapter is not a missing policy.
pub fn handler_support(f: &Value) -> Value {
    use serverless_core::HandlerId;
    let app = light_lambda::input(None, false).expect("offline app declarations");
    let mut registered = app
        .handlers
        .iter()
        .filter(|h| {
            h.factory
                .as_ref()
                .is_some_and(|f| f.application_available())
        })
        .map(|h| h.id)
        .collect::<BTreeSet<_>>();
    registered.extend([HandlerId::Proxy, HandlerId::Security, HandlerId::Jwt]);
    let mut ready = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut deferred = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    let mut staged = BTreeSet::new();
    for raw in f
        .pointer("/config/files/handler/handlers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let class = raw.split('@').next().unwrap_or(raw);
        match serverless_core::compat::lookup(class) {
            Some(c) if c.disposition == serverless_core::compat::Disposition::Deferred => {
                deferred.insert(c.alias);
            }
            Some(c) if registered.contains(&c.id) => {
                ready.insert(c.alias);
            }
            Some(c) => {
                missing.insert(c.alias);
                if app
                    .handlers
                    .iter()
                    .any(|h| h.id == c.id && h.factory.is_some())
                {
                    staged.insert(c.alias);
                }
            }
            None => {
                unknown.insert(class);
            }
        }
    }
    json!({"registered":ready,"unregistered":missing,"factoryWiredActivationClosed":staged,"deferred":deferred,"unknownClasses":unknown,"scope":"declared classes, including disabled/unselected declarations; legacy unregistered axis includes wired factories whose application activation is closed; not a count of absent production features"})
}

pub fn runner_gaps(f: &Value) -> Vec<String> {
    if crate::router_failed_component::supported(f) {
        return crate::router_failed_component::validate(f)
            .err()
            .map(|e| vec![format!("invalid Router component: {e:#}")])
            .unwrap_or_default();
    }
    if crate::gate_component::supported(f) {
        return crate::gate_component::validate(f)
            .err()
            .map(|e| vec![format!("invalid gate component: {e:#}")])
            .unwrap_or_default();
    }
    if crate::single_runner::supported(f) {
        return vec![];
    }
    let mut gaps = BTreeSet::new();
    let allowed = [
        "schema",
        "id",
        "title",
        "ledger",
        "config",
        "clock",
        "keys",
        "tokens",
        "headerMode",
        "nondeterministic",
        "javaOnly",
        "rustOnly",
        "differences",
        "rawEvent",
        "backend",
        "expect",
        "expectJava",
        "substitutions",
        "steps",
        "dependencies",
        "expectInit",
        "expectJavaInit",
        "expectRustInit",
        "initOnly",
        "jwtCachePrerequisite",
        "loopbackJwksPrerequisite",
        "loopbackOauthPrerequisite",
        "ruleConfigurationPair",
        "context",
        "runtimeIdentity",
        "insecureTestConfigPrerequisite",
        "jwksPublishedKeys",
        "securityLogProbe",
        "boundedWorkProbe",
        "responsePolicyProbe",
        "requestFailureProbe",
        "observeBackendDiagnostics",
        "ruleFailureProbe",
        "ruleInputProbe",
        "concurrentCopies",
        "configurationProbe",
        "dynamoProbe",
        "traceContextProbe",
        "completionProbe",
        "blockingLeasePrerequisite",
    ];
    for key in f.as_object().into_iter().flat_map(|o| o.keys()) {
        if !allowed.contains(&key.as_str()) {
            gaps.insert(format!("fixture prerequisite/probe:{key}"));
        }
    }
    for case in std::iter::once(f).chain(crate::application::cases(f)) {
        if let Some(context) = case.get("context")
            && !crate::application::valid_context(context)
        {
            gaps.insert("unsupported Lambda context recipe".into());
        }
    }
    if let Some(identity) = f.get("runtimeIdentity")
        && (identity.as_object().is_none_or(|v| v.len() != 1)
            || identity["functionName"].as_str().is_none_or(str::is_empty)
            || f.get("context").is_some())
    {
        gaps.insert("unsupported or ambiguous runtime identity".into());
    }
    if f.get("ruleConfigurationPair").is_some()
        && (f["ruleConfigurationPair"]["rust"]["executor"] != "light-rule-cel"
            || !f["ruleConfigurationPair"]["rust"]["rule"].is_object()
            || f["config"]["files"].get("rule").is_some())
    {
        gaps.insert("unsupported or ambiguous immutable rule recipe".into());
    }
    if f["javaOnly"] == true || f["rustOnly"] == true {
        gaps.insert("single-runner result/skipped contract".into());
    }
    if crate::test_config_flag::validate(f).is_err() {
        gaps.insert("invalid test-config flag prerequisite".into());
    }
    gaps.extend(crate::tokens::gaps(f));
    if let Some(dependencies) = f["dependencies"].as_object() {
        for (key, value) in dependencies {
            if !(["http", "jwks"].contains(&key.as_str())
                || (key == "ldap"
                    && f["id"] == "S-16-loopback-bind-denial-represented-v1"
                    && value["bindResult"] == "invalidCredentials")
                || (key == "invoke"
                    && (matches!(
                        f["id"].as_str(),
                        Some(
                            "D-08-zero-remaining-multi-contract-v1"
                                | "D-08-zero-remaining-supported-config-v1"
                        )
                    ) || f["id"] == "CFG-02-sdk-netty-trust-boundary-represented-v1")))
                || (value.get("tls").is_some()
                    && !(value["tls"] == true
                        && ((key == "http"
                            && f["id"] == "CFG-02-loopback-jks-tls-represented-v1")
                            || (key == "invoke"
                                && f["id"] == "CFG-02-sdk-netty-trust-boundary-represented-v1"))))
                || (value.get("refused").is_some()
                    && !(key == "http"
                        && value["refused"] == true
                        && value["responses"] == json!([])))
            {
                gaps.insert(format!("loopback dependency:{key} profile"));
            }
            if value["responses"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r.get("relativeIntrospectionExpSec").is_some())
            {
                gaps.insert("relative introspection fixture recipe".into());
            }
        }
    }
    if f["nondeterministic"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
        && !crate::legacy_correlation::supported(f)
    {
        gaps.insert("declared nondeterministic assertions".into());
    }
    let support = handler_support(f);
    if support["unknownClasses"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        gaps.insert("custom fixture handler/probe factory".into());
    }
    for case in crate::application::cases(f) {
        if f["initOnly"] != true && !case.get("rawEvent").is_some_and(Value::is_object) {
            gaps.insert("shared ALB request generator".into());
        }
        if !std::ptr::eq(case, f) && case["context"].get("functionName").is_some() {
            gaps.insert("warm Lambda identity cannot change after Init".into());
        }
        if !std::ptr::eq(case, f) {
            for key in case.as_object().into_iter().flat_map(|o| o.keys()) {
                if ![
                    "rawEvent",
                    "backend",
                    "expect",
                    "expectJava",
                    "headerMode",
                    "waitBeforeMs",
                    "warmConfig",
                    "jwksPublishedKeys",
                    "observeAudit",
                    "context",
                    "cacheActions",
                ]
                .contains(&key.as_str())
                {
                    gaps.insert(format!("warm-step recipe:{key}"));
                }
            }
        }
        if ![
            "response",
            "initError",
            "initSuccess",
            "notExecuted",
            "parallelResponses",
        ]
        .contains(&case["expect"]["outcome"].as_str().unwrap_or(""))
        {
            gaps.insert("non-response expectation adapter".into());
        }
        for side in ["expect", "expectJava"] {
            if let Some(expect) = case[side].as_object() {
                for key in expect.keys() {
                    if ![
                        "outcome",
                        "status",
                        "headers",
                        "headersAbsent",
                        "body",
                        "bodyBase64",
                        "backendCalls",
                        "jwksFetches",
                        "jwksCalls",
                        "responseMaps",
                        "httpCalls",
                        "code",
                        "auditRecords",
                        "reason",
                        "class",
                        "phase",
                        "message",
                        "invocationTiming",
                        "configurationWarningObservation",
                        "securityLogObservation",
                        "boundedWorkProbeExecutions",
                        "responsePolicyObservation",
                        "failureProbeResult",
                        "initFailureReporting",
                        "emf",
                        "configurationDiagnostics",
                        "backendDiagnostics",
                        "ruleFailuresCaught",
                        "ruleInputObservation",
                        "responses",
                        "configurationObservation",
                        "cacheActions",
                        "dynamoObservation",
                        "ldapBinds",
                        "traceContext",
                        "completionProbeResult",
                        "blockingLease",
                    ]
                    .contains(&key.as_str())
                    {
                        gaps.insert(format!("assertion:{key}"));
                    }
                }
                for call in case[side]["backendCalls"].as_array().into_iter().flatten() {
                    for key in call.as_object().into_iter().flat_map(|o| o.keys()) {
                        if ![
                            "target",
                            "event",
                            "eventExact",
                            "eventAbsent",
                            "invoke",
                            "eventClock",
                            "eventCorrelation",
                            "eventAuthorizer",
                            "sdkHttp",
                        ]
                        .contains(&key.as_str())
                        {
                            gaps.insert(format!("backend assertion:{key}"));
                        }
                    }
                }
            }
        }
        for replay in case["backend"].as_array().into_iter().flatten() {
            for key in replay["respond"]
                .as_object()
                .into_iter()
                .flat_map(|o| o.keys())
            {
                if key == "sdkFailure" {
                    let failure = &replay["respond"]["sdkFailure"];
                    if failure["kind"] == "SdkClientException" {
                        if failure.as_object().is_none_or(|o| o.len() != 2)
                            || failure["message"] != "test-only-transport"
                        {
                            gaps.insert("Invoke client-exception wire recipe".into());
                        }
                    } else if ![
                        "TooManyRequestsException",
                        "ResourceNotFoundException",
                        "LambdaException",
                    ]
                    .contains(&failure["kind"].as_str().unwrap_or(""))
                        || failure["statusCode"].as_u64().is_none()
                    {
                        gaps.insert("Invoke client-exception wire recipe".into());
                    }
                } else if !["functionError", "payload", "logResult", "executedVersion"]
                    .contains(&key.as_str())
                {
                    gaps.insert(format!("Invoke wire recipe:{key}"));
                }
            }
        }
    }
    gaps.into_iter().collect()
}

fn write_new(path: &Path, value: &Value) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    Ok(())
}

#[derive(Default)]
struct Options {
    original_expectations: bool,
    original_declarations: bool,
    fixtures: PathBuf,
    out: PathBuf,
    java: PathBuf,
    rust: PathBuf,
    ids: BTreeSet<String>,
    handlers: BTreeSet<String>,
    profile: Option<String>,
    retained_conflict: Option<PathBuf>,
    retained_results: Option<PathBuf>,
}
fn options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        fixtures: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures"),
        ..Options::default()
    };
    ensure!(args.len() % 2 == 1, "options require values");
    for pair in args[1..].as_chunks::<2>().0 {
        match pair[0].as_str() {
            "--expectations" if pair[1] == "original" => o.original_expectations = true,
            "--expectations" if pair[1] == "approved-v1" => o.original_expectations = false,
            "--declarations" if pair[1] == "original" => o.original_declarations = true,
            "--declarations" if pair[1] == "reviewed" => o.original_declarations = false,
            "--fixtures" => o.fixtures = pair[1].clone().into(),
            "--out" => o.out = pair[1].clone().into(),
            "--java" => o.java = pair[1].clone().into(),
            "--rust" => o.rust = pair[1].clone().into(),
            "--retain-conflict" => o.retained_conflict = Some(pair[1].clone().into()),
            "--retain-results" => o.retained_results = Some(pair[1].clone().into()),
            "--fixture" => {
                o.ids.insert(pair[1].clone());
            }
            "--handler" => {
                o.handlers.insert(pair[1].clone());
            }
            "--profile"
                if ["registered-v3", "router-forwarding-v1"].contains(&pair[1].as_str()) =>
            {
                o.profile = Some(pair[1].clone());
            }
            "--profile" if pair[1] == "registered-v2" => {
                o.ids.extend(SUPPORTED.iter().map(|id| (*id).to_owned()));
            }
            _ => bail!("unknown option {}", pair[0]),
        }
    }
    ensure!(
        !o.out.as_os_str().is_empty(),
        "--out required (must not exist)"
    );
    if args[0] == "compare" {
        ensure!(
            !o.java.as_os_str().is_empty() && !o.rust.as_os_str().is_empty(),
            "--java and --rust required"
        );
    }
    Ok(o)
}

fn handlers(f: &Value) -> BTreeSet<String> {
    f.pointer("/config/files/handler/handlers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|s| serverless_core::compat::lookup(s.split('@').next().unwrap_or(s)))
        .map(|c| c.alias.to_owned())
        .collect()
}

fn profile_gaps(f: &Value) -> Vec<&'static str> {
    let mut gaps = vec![];
    for value in [
        f.pointer("/config/files/router/urlRewriteRules"),
        f.pointer("/config/values/router.urlRewriteRules"),
    ]
    .into_iter()
    .flatten()
    {
        if value.as_array().is_some_and(|a| !a.is_empty())
            || value.as_str().is_some_and(|s| !s.trim().is_empty())
        {
            gaps.push("Router rewrite execution unavailable; configuration rejects Init");
            break;
        }
    }
    gaps
}

pub fn command(args: &[String]) -> Result<()> {
    let o = options(args)?;
    let mut inventory = vec![];
    let mut ids = BTreeSet::new();
    for path in crate::fixture_files(&o.fixtures)? {
        let f = crate::load(&path)?;
        let id = f["id"].as_str().context("fixture id")?.to_owned();
        ensure!(ids.insert(id.clone()), "duplicate fixture {id}");
        inventory.push((id, f));
    }
    for id in &o.ids {
        ensure!(ids.contains(id), "unknown fixture {id}");
    }
    for alias in &o.handlers {
        ensure!(
            serverless_core::compat::COMPATIBILITY
                .iter()
                .any(|c| c.alias == alias),
            "unknown native handler {alias}"
        );
    }
    std::fs::create_dir(&o.out).context("output must be a fresh directory")?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let fixture_schema = crate::load(&root.join("schema/fixture-v1.schema.json"))?;
    let result_schema = crate::load(&root.join("schema/result-v1.schema.json"))?;
    let ledger = crate::ledger_rows(&std::fs::read_to_string(root.join("LEDGER.md"))?)?;
    let revision = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()?
            .stdout,
    )?
    .trim()
    .to_owned();
    let retained = o
        .retained_conflict
        .as_ref()
        .map(|path| -> Result<Value> {
            ensure!(args[0] == "run", "retained conflict is only a run input");
            let result = crate::load(path)?;
            crate::schema_validate(&result_schema, &result)?;
            ensure!(
                result["id"] == "H-04-transport-refused-multi-contract-v1"
                    && result["runner"] == "rust"
                    && result["execution"] == "failed"
                    && result["observations"][0]["outcome"] == "initError"
                    && result["observations"][0]["code"] == "InvalidConfiguration",
                "only the preserved H-04 activation conflict may be retained"
            );
            Ok(result)
        })
        .transpose()?;
    let rt = tokio::runtime::Runtime::new()?;
    let mut rows = vec![];
    let (
        mut selected,
        mut passes,
        mut failures,
        mut runner_missing,
        mut handler_missing,
        mut profile_missing,
    ) = (0, 0, 0, 0, 0, 0);
    let mut expectation_passes = 0;
    let mut expectation_failures = 0;
    let mut deferred_missing = 0;
    for (id, f) in &inventory {
        let aliases = handlers(f);
        let support = handler_support(f);
        let gaps = runner_gaps(f);
        let profiles = profile_gaps(f);
        let missing = !support["unregistered"].as_array().unwrap().is_empty()
            || !support["deferred"].as_array().unwrap().is_empty();
        let by_profile = match o.profile.as_deref() {
            Some("registered-v3") => gaps.is_empty() && !missing && profiles.is_empty(),
            Some("router-forwarding-v1") => id.starts_with("H-03-forwarding-no-rewrite-"),
            _ => false,
        };
        let chosen = (o.profile.is_none() && o.ids.is_empty() && o.handlers.is_empty())
            || o.ids.contains(id)
            || !aliases.is_disjoint(&o.handlers)
            || by_profile;
        let kind = if f["javaOnly"] == true || f["rustOnly"] == true {
            "single-runner"
        } else if f.get("expectJava").is_some()
            || crate::application::cases(f)
                .iter()
                .any(|s| s.get("expectJava").is_some())
        {
            "intentional-difference"
        } else {
            "equivalent"
        };
        let mut errors = vec![];
        let mut failure_kind = "runner-or-input-error";
        let mut difference_detail = Value::Null;
        let mut declaration_review = Value::Null;
        let mut java_execution = Value::Null;
        let mut rust_execution = Value::Null;
        let mut coverage_kind = Value::Null;
        let mut retained_result_evidence = Value::Null;
        let amendment = if o.original_expectations {
            None
        } else {
            crate::amendments::review(f)?
        };
        let outcome = if !chosen {
            "unselected"
        } else {
            selected += 1;
            if !gaps.is_empty() {
                runner_missing += 1;
                "runner-unsupported"
            } else if !support["unregistered"].as_array().unwrap().is_empty() {
                handler_missing += 1;
                "handler-unregistered"
            } else if !support["deferred"].as_array().unwrap().is_empty()
                && !crate::single_runner::java_component(f)
            {
                deferred_missing += 1;
                "handler-deferred"
            } else {
                let check = (|| -> Result<()> {
                    crate::schema_validate(&fixture_schema, f)?;
                    crate::cross_validate(f, &ledger)?;
                    if args[0] == "run" {
                        let prior_path = o
                            .retained_results
                            .as_ref()
                            .map(|dir| dir.join(format!("{id}.json")))
                            .filter(|path| path.is_file());
                        let prior = prior_path
                            .as_ref()
                            .map(|path| crate::load(path))
                            .transpose()?;
                        let result = if let Some(result) =
                            retained.as_ref().filter(|r| r["id"] == *id)
                        {
                            ensure!(
                                result["fixtureDigest"] == crate::fixture_digest(f)?,
                                "stale retained conflict digest"
                            );
                            result.clone()
                        } else if let Some(result) =
                            prior.as_ref().filter(|r| r["execution"] == "passed")
                        {
                            crate::schema_validate(&result_schema, result)?;
                            verify_retained(f, result, o.original_expectations)?;
                            let path = prior_path.as_ref().unwrap();
                            retained_result_evidence = json!({"path":path,"sha256":hex::encode(Sha256::digest(std::fs::read(path)?)),"meaning":"retained execution; assertions rechecked against current fixture and active expectations"});
                            result.clone()
                        } else if o.original_expectations {
                            rt.block_on(crate::application::run(f, &revision))?
                        } else {
                            rt.block_on(run_fixture(f, &revision))?
                        };
                        crate::schema_validate(&result_schema, &result)?;
                        rust_execution = result["execution"].clone();
                        coverage_kind = json!(if crate::router_failed_component::supported(f) {
                            "paired-handler-component"
                        } else if crate::single_runner::supported(f) {
                            crate::single_runner::coverage(f)
                        } else {
                            crate::amendments::coverage(&result)
                        });
                        write_new(&o.out.join(format!("{id}.json")), &result)?;
                        failure_kind = "rust-expectation-mismatch";
                        if crate::single_runner::java_component(f) {
                            ensure!(
                                result["execution"] == "skipped",
                                "Java-only counterpart must skip"
                            );
                        } else if profiles.is_empty() && result["execution"] == "passed" {
                            expectation_passes += 1;
                        } else if profiles.is_empty() {
                            expectation_failures += 1;
                        }
                        ensure!(
                            result["execution"] == "passed"
                                || (crate::single_runner::java_component(f)
                                    && result["execution"] == "skipped"),
                            "Rust expectation failed: {}",
                            result["assertionErrors"]
                        );
                    } else {
                        failure_kind = "missing-java-evidence";
                        let j = find_result(&o.java, id)?;
                        failure_kind = "missing-rust-evidence";
                        let r = find_result(&o.rust, id)?;
                        java_execution = j["execution"].clone();
                        rust_execution = r["execution"].clone();
                        coverage_kind = json!(if crate::router_failed_component::supported(f) {
                            "paired-handler-component"
                        } else if crate::single_runner::supported(f) {
                            crate::single_runner::coverage(f)
                        } else {
                            crate::amendments::coverage(&r)
                        });
                        failure_kind = "invalid-result-evidence";
                        for (runner, result) in [("java", &j), ("rust", &r)] {
                            crate::schema_validate(&result_schema, result)?;
                            ensure!(
                                result["runner"] == runner
                                    && result["fixtureDigest"] == crate::fixture_digest(f)?,
                                "stale/wrong-runner result {id}"
                            );
                        }
                        failure_kind = if r["execution"] != "passed" {
                            "rust-expectation-mismatch"
                        } else if j["execution"] != "passed" {
                            "java-expectation-mismatch"
                        } else {
                            "expectation-mismatch"
                        };
                        if !o.original_expectations {
                            if !o.original_declarations {
                                declaration_review =
                                    crate::declarations::review(f)?.unwrap_or(Value::Null);
                            }
                            crate::amendments::compare(f, &j, &r, o.original_declarations)?;
                        } else if o.original_declarations {
                            compare(f, &j, &r)?;
                        } else {
                            declaration_review =
                                crate::declarations::review(f)?.unwrap_or(Value::Null);
                            crate::declarations::compare(f, &j, &r)?;
                        }
                    }
                    Ok(())
                })();
                match check {
                    Ok(()) if profiles.is_empty() => {
                        passes += 1;
                        "pass"
                    }
                    Ok(()) => {
                        profile_missing += 1;
                        "handler-profile-incomplete"
                    }
                    Err(error) => {
                        if let Some(d) = error.downcast_ref::<DifferentialMismatch>() {
                            failure_kind = if d.unexpected.is_empty() {
                                "declaration-conflict"
                            } else if d.stale.is_empty() {
                                "differential-mismatch"
                            } else {
                                "differential-mismatch-and-declaration-conflict"
                            };
                            difference_detail = json!({"observed":d.observed,"declared":d.declared,"unexpected":d.unexpected,"stale":d.stale});
                        }
                        errors.push(format!("{error:#}"));
                        if profiles.is_empty() {
                            failures += 1;
                            "fail"
                        } else {
                            profile_missing += 1;
                            "handler-profile-incomplete"
                        }
                    }
                }
            }
        };
        if chosen {
            println!("{id}\t{kind}\t{outcome}");
        }
        rows.push(json!({"id":id,"coverageKind":coverage_kind,"retainedResultEvidence":retained_result_evidence,"retainedConflictEvidence":if retained.as_ref().is_some_and(|r|r["id"]==*id){o.retained_conflict.as_ref()}else{None},"rustExpectationAmendment":amendment,"originalDeclaredDifferences":f["differences"],"kind":kind,"selected":chosen,"adapterImplemented":gaps.is_empty(),"runnerSupport":if gaps.is_empty(){"implemented"}else{"missing"},"runnerGaps":gaps,"handlerSupport":support,"handlerProfileGaps":profiles,"handlers":aliases,"outcome":outcome,"failureKind":if outcome=="fail"{Some(failure_kind)}else{None},"differenceDetail":difference_detail,"declarationReview":declaration_review,"javaExecution":java_execution,"rustExecution":rust_execution,"errors":errors}));
    }
    let incomplete = runner_missing + handler_missing + deferred_missing + profile_missing;
    let unselected = inventory.len() - selected;
    let unsupported = rows
        .iter()
        .filter(|r| r["runnerSupport"] == "missing")
        .map(|r| r["id"].clone())
        .collect::<Vec<_>>();
    let handler_inventory=serverless_core::compat::COMPATIBILITY.iter().map(|c|{
        let relevant=rows.iter().filter(|r|r["handlers"].as_array().is_some_and(|a|a.iter().any(|h|h==c.alias))).collect::<Vec<_>>();
        json!({"class":c.class,"alias":c.alias,"disposition":format!("{:?}",c.disposition),"fixtureCount":relevant.len(),"selected":relevant.iter().filter(|r|r["selected"]==true).count(),"pass":relevant.iter().filter(|r|r["outcome"]=="pass").count(),"fail":relevant.iter().filter(|r|r["outcome"]=="fail").count(),"adapterUnimplemented":relevant.iter().filter(|r|r["runnerSupport"]=="missing").count()})
    }).collect::<Vec<_>>();
    let complete = selected > 0 && failures == 0 && incomplete == 0;
    let report = json!({"schema":"light-lambda-incremental-coverage/v2","action":args[0],"selection":{"fixtures":o.ids,"handlers":o.handlers,"profile":o.profile},"total":inventory.len(),"selected":selected,"pass":passes,"fail":failures,"expectationPasses":if args[0]=="run"{Some(expectation_passes)}else{None},"expectationFailures":if args[0]=="run"{Some(expectation_failures)}else{None},"differentialPasses":if args[0]=="compare"{Some(passes)}else{None},"runnerUnsupported":runner_missing,"selectedUnregisteredHandlers":handler_missing,"selectedIncompleteHandlerProfiles":profile_missing,"unimplemented":incomplete,"unimplementedMeaning":"selected incomplete fixture executions; see separate runner, registration and profile counts","unselected":unselected,"runnerUnsupportedInventory":unsupported,"handlerInventory":handler_inventory,"fixturesDeclaringUnregisteredHandlers":rows.iter().filter(|r|!r["handlerSupport"]["unregistered"].as_array().unwrap().is_empty()).count(),"rows":rows,"complete":complete,"inventoryComplete":complete && unselected==0,"completionScope":"selected fixtures only; never full handler or AWS parity","applicationProfileRestrictions":{"router":"forwarding only; nonempty rewrite configuration rejects Init","validator":"factory wired; application activation closed pending precision/executor/failure admission gates"}});
    let mut report = report;
    let component_passes = rows
        .iter()
        .filter(|r| r["outcome"] == "pass" && r["kind"] == "single-runner")
        .count();
    report["singleRunnerPasses"] = json!(component_passes);
    if args[0] == "compare" {
        report["differentialPasses"] = json!(passes - component_passes);
    }
    report["rustExpectationMode"] = json!(if o.original_expectations {
        "original"
    } else {
        "approved-v1+v2+v3+v4+v5"
    });
    report["admissionOnlyPasses"] = json!(
        rows.iter()
            .filter(|r| r["outcome"] == "pass" && r["coverageKind"] == "admission-only")
            .count()
    );
    report["serviceExecutionPasses"] = json!(
        rows.iter()
            .filter(|r| r["outcome"] == "pass" && r["coverageKind"] == "service-execution")
            .count()
    );
    report["selectedDeferredHandlers"] = json!(deferred_missing);
    let mut failure_kinds = BTreeMap::<String, usize>::new();
    for row in &rows {
        if let Some(kind) = row["failureKind"].as_str() {
            *failure_kinds.entry(kind.to_owned()).or_default() += 1;
        }
    }
    report["failureKinds"] = json!(failure_kinds);
    report["declarationMode"] = json!(if o.original_declarations {
        "original"
    } else {
        "reviewed"
    });
    report["javaExpectationFailures"] = if args[0] == "compare" {
        json!(
            rows.iter()
                .filter(|r| r["javaExecution"] == "failed")
                .count()
        )
    } else {
        Value::Null
    };
    report["declarationCorrectionsApplied"] = json!(
        rows.iter()
            .filter(|r| r["selected"] == true && r["declarationReview"].is_object())
            .count()
    );
    report["sourceDeclarationConflictsSupersededByApprovedExpectation"] = json!(
        rows.iter()
            .filter(|r| {
                r["rustExpectationAmendment"].is_object()
                    && r["originalDeclaredDifferences"]
                        != r["rustExpectationAmendment"]["approvedExpectationDifferenceFields"]
            })
            .count()
    );
    report["declarationConflicts"] = json!(
        rows.iter()
            .filter(|r| r["differenceDetail"]["stale"]
                .as_array()
                .is_some_and(|a| !a.is_empty()))
            .count()
    );
    report["differentialMismatches"] = json!(
        rows.iter()
            .filter(|r| r["differenceDetail"]["unexpected"]
                .as_array()
                .is_some_and(|a| !a.is_empty()))
            .count()
    );
    report["rustExpectationFailures"] = json!(
        rows.iter()
            .filter(|r| r["failureKind"] == "rust-expectation-mismatch")
            .count()
    );
    crate::gap_work::annotate(&mut report, &inventory)?;
    write_new(&o.out.join("coverage.json"), &report)?;
    println!(
        "selected={selected}/{} pass={passes} fail={failures} runnerGaps={runner_missing} unregistered={handler_missing} deferred={deferred_missing} incompleteProfiles={profile_missing} unselected={unselected}",
        inventory.len()
    );
    ensure!(
        complete,
        "incremental run incomplete or failed; see coverage.json"
    );
    Ok(())
}

fn find_result(dir: &Path, id: &str) -> Result<Value> {
    let mut matches = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let value = crate::load(&path)?;
            if value["schema"] == "light-lambda-conformance/result/v1" && value["id"] == id {
                matches.push(value);
            }
        }
    }
    ensure!(
        matches.len() == 1,
        "expected exactly one result for {id}, found {}",
        matches.len()
    );
    Ok(matches.remove(0))
}

pub async fn run_fixture(f: &Value, revision: &str) -> Result<Value> {
    ensure!(
        runner_gaps(f).is_empty(),
        "unsupported fixture adapter: {:?}",
        runner_gaps(f)
    );
    crate::amendments::run(f, revision).await
}

/// Reproduce the original Rust contract without applying owner amendments.
pub async fn run_fixture_original(f: &Value, revision: &str) -> Result<Value> {
    ensure!(
        runner_gaps(f).is_empty(),
        "unsupported fixture adapter: {:?}",
        runner_gaps(f)
    );
    crate::application::run(f, revision).await
}

fn subset(expected: &Value, actual: &Value, path: &str) -> Result<()> {
    match expected {
        Value::Object(map) => {
            ensure!(actual.is_object(), "{path}: expected object");
            for (k, v) in map {
                subset(
                    v,
                    actual
                        .get(k)
                        .with_context(|| format!("{path}/{k}: absent"))?,
                    &format!("{path}/{k}"),
                )?;
            }
        }
        Value::Array(values) => {
            let a = actual
                .as_array()
                .with_context(|| format!("{path}: expected array"))?;
            ensure!(values.len() == a.len(), "{path}: array length");
            for (i, (e, a)) in values.iter().zip(a).enumerate() {
                subset(e, a, &format!("{path}/{i}"))?;
            }
        }
        _ => ensure!(
            expected == actual,
            "{path}: expected {expected}, got {actual}"
        ),
    }
    Ok(())
}
fn header_map(v: &Value) -> Result<BTreeMap<String, Vec<Value>>> {
    let mut map = BTreeMap::<String, Vec<Value>>::new();
    for pair in v.as_array().context("header pairs")? {
        let p = pair.as_array().context("header pair")?;
        ensure!(p.len() == 2, "header pair length");
        map.entry(p[0].as_str().context("header name")?.to_ascii_lowercase())
            .or_default()
            .push(p[1].clone());
    }
    Ok(map)
}
pub fn assert_observation(e: &Value, a: &Value) -> Result<()> {
    assert_observation_with_window(e, a, None)
}
pub(crate) fn assert_observation_with_window(
    e: &Value,
    a: &Value,
    window: Option<&Value>,
) -> Result<()> {
    if let Some(responses) = e.get("responses") {
        let expected = responses
            .as_array()
            .context("expected parallel responses")?;
        let actual = a["responses"]
            .as_array()
            .context("actual parallel responses")?;
        ensure!(expected.len() == actual.len(), "parallel response count");
        for (expected, actual) in expected.iter().zip(actual) {
            let mut expected = expected.clone();
            let mut actual = actual.clone();
            expected["backendCalls"] = json!([]);
            actual["backendCalls"] = json!([]);
            expected["headersAbsent"] = json!([]);
            assert_observation(&expected, &actual)?;
        }
    }
    if matches!(
        e["outcome"].as_str(),
        Some("jwksGateCancellation" | "jwksGatePanicRecovery")
    ) {
        return subset(e, a, "actual gate component");
    }
    let http_calls = crate::recipes::http_calls(&e["httpCalls"], &a["httpCalls"]);
    let prepared = crate::recipes::prepare(e, a, window)?;
    let e = &prepared;
    if e["outcome"] == "initError" || e["outcome"] == "initSuccess" {
        for key in e.as_object().context("Init expectation")?.keys() {
            ensure!(
                [
                    "outcome",
                    "code",
                    "backendCalls",
                    "jwksFetches",
                    "httpCalls",
                    "jwksCalls",
                    "configurationWarningObservation",
                    "initFailureReporting",
                    "configurationObservation",
                    "dynamoObservation",
                    "ldapBinds"
                ]
                .contains(&key.as_str()),
                "unimplemented Init assertion {key}"
            );
        }
        for key in [
            "outcome",
            "code",
            "backendCalls",
            "jwksFetches",
            "jwksCalls",
            "configurationWarningObservation",
            "initFailureReporting",
            "configurationObservation",
            "dynamoObservation",
            "ldapBinds",
            "traceContext",
            "completionProbeResult",
            "blockingLease",
        ] {
            if e.get(key).is_none() {
                continue;
            }
            ensure!(e.get(key) == a.get(key), "Init {key} mismatch");
        }
        if let Some(calls) = e.get("httpCalls") {
            subset(calls, &a["httpCalls"], "Init HTTP calls")?;
        }
        return Ok(());
    }
    if e["outcome"] == "notExecuted" {
        let allowed = &[
            "outcome",
            "reason",
            "backendCalls",
            "jwksFetches",
            "httpCalls",
            "auditRecords",
            "cacheActions",
        ][..];
        for (key, value) in e.as_object().context("non-response expectation")? {
            ensure!(
                allowed.contains(&key.as_str()),
                "unimplemented non-response assertion {key}"
            );
            ensure!(
                a.get(key) == Some(value),
                "{key}: expected {value}, got {}",
                a[key]
            );
        }
        return Ok(());
    }
    let known = [
        "outcome",
        "status",
        "headers",
        "headersAbsent",
        "body",
        "bodyBase64",
        "backendCalls",
        "jwksFetches",
        "jwksCalls",
        "httpCalls",
        "responseMaps",
        "auditRecords",
        "class",
        "phase",
        "message",
        "invocationTiming",
        "securityLogObservation",
        "boundedWorkProbeExecutions",
        "responsePolicyObservation",
        "failureProbeResult",
        "emf",
        "configurationDiagnostics",
        "backendDiagnostics",
        "ruleFailuresCaught",
        "ruleInputObservation",
        "responses",
        "cacheActions",
        "ldapBinds",
        "traceContext",
        "completionProbeResult",
        "blockingLease",
    ];
    if let Some(diagnostics) = e.get("backendDiagnostics") {
        crate::backend_diagnostics::assert(diagnostics, &a["backendDiagnostics"])?;
    }
    if let Some(emf) = e.get("emf") {
        crate::emission_worker::assert(emf, &a["emf"])?;
    }
    if let Some(timing) = e.get("invocationTiming") {
        let duration = a["invocationDurationMs"]
            .as_u64()
            .context("measured invocation duration required")?;
        let minimum = timing["minimumMs"].as_u64().context("minimum duration")?;
        let maximum = timing["maximumMs"].as_u64().context("maximum duration")?;
        ensure!(
            minimum <= duration && duration <= maximum,
            "invocation duration {duration}ms outside {minimum}..={maximum}ms"
        );
    }
    for key in e.as_object().context("expectation")?.keys() {
        ensure!(
            known.contains(&key.as_str()),
            "unimplemented assertion {key}"
        );
    }
    for key in [
        "class",
        "phase",
        "message",
        "outcome",
        "status",
        "body",
        "bodyBase64",
        "jwksFetches",
        "jwksCalls",
        "securityLogObservation",
        "boundedWorkProbeExecutions",
        "responsePolicyObservation",
        "failureProbeResult",
        "configurationDiagnostics",
        "ruleFailuresCaught",
        "ruleInputObservation",
        "ldapBinds",
        "traceContext",
        "completionProbeResult",
        "blockingLease",
    ] {
        if let Some(v) = e.get(key) {
            ensure!(a.get(key) == Some(v), "{key}: expected {v}, got {}", a[key]);
        }
    }
    if let Some(records) = e.get("auditRecords") {
        let expected = records.as_array().context("expected audit records")?;
        let actual = a["auditRecords"]
            .as_array()
            .context("actual audit records")?;
        ensure!(
            expected.len() == actual.len(),
            "exact audit record count mismatch"
        );
        for (expected, actual) in expected.iter().zip(actual) {
            ensure!(
                actual["data"] == expected["data"],
                "exact audit data mismatch"
            );
            let message: Value =
                serde_json::from_str(actual["message"].as_str().context("actual audit message")?)?;
            ensure!(message == actual["data"], "audit message/data disagree");
            if let Some(message) = expected.get("message") {
                ensure!(
                    actual.get("message") == Some(message),
                    "exact audit message mismatch"
                );
            }
            ensure!(
                actual
                    .as_object()
                    .context("audit record object")?
                    .keys()
                    .all(|k| ["data", "message"].contains(&k.as_str())),
                "unexpected audit record field"
            );
        }
    }
    if e["outcome"] == "response" {
        let headers = header_map(&a["headers"])?;
        ensure!(
            headers == header_map(&e["headers"])?,
            "response header multimap mismatch"
        );
        for name in e["headersAbsent"].as_array().context("headersAbsent")? {
            ensure!(
                !headers
                    .contains_key(&name.as_str().context("absent header")?.to_ascii_lowercase()),
                "header must be absent"
            );
        }
    }
    if let Some(maps) = e.get("responseMaps") {
        ensure!(
            a.get("responseMaps") == Some(maps),
            "exact response namespaces differ"
        );
    }
    if let Some(calls) = e.get("httpCalls") {
        subset(calls, &http_calls, "httpCalls")?;
    }
    let ec = e["backendCalls"].as_array().context("expected calls")?;
    let ac = a["backendCalls"].as_array().context("actual calls")?;
    ensure!(ec.len() == ac.len(), "backend call count mismatch");
    for (i, (e, a)) in ec.iter().zip(ac).enumerate() {
        for key in e.as_object().context("expected call")?.keys() {
            ensure!(
                [
                    "target",
                    "event",
                    "eventExact",
                    "eventAbsent",
                    "invoke",
                    "sdkHttp"
                ]
                .contains(&key.as_str()),
                "unimplemented backend assertion {key}"
            );
        }
        ensure!(e["target"] == a["target"], "backend {i} target mismatch");
        if let Some(invoke) = e.get("invoke") {
            subset(invoke, &a["invoke"], "Invoke wire settings")?;
        }
        if let Some(sdk_http) = e.get("sdkHttp") {
            let mut sdk_http = sdk_http.clone();
            if sdk_http["endpoint"] == "${dependency:invoke}" {
                let actual = a["sdkHttp"]["endpoint"]
                    .as_str()
                    .context("observed SDK endpoint")?;
                let url = url::Url::parse(actual)?;
                ensure!(
                    url.scheme() == "http"
                        && url.host_str() == Some("127.0.0.1")
                        && url.port().is_some()
                        && url.path() == "/"
                        && url.query().is_none(),
                    "owned SDK endpoint required"
                );
                sdk_http["endpoint"] = json!(actual);
            }
            subset(&sdk_http, &a["sdkHttp"], "actual SDK HTTP attempt")?;
        }
        subset(&e["event"], &a["event"], &format!("backend/{i}/event"))?;
        if let Some(exact) = e["eventExact"].as_object() {
            for (key, v) in exact {
                ensure!(
                    a["event"].get(key) == Some(v),
                    "backend {i} exact {key} mismatch"
                );
            }
        }
        if let Some(absent) = e["eventAbsent"].as_array() {
            for path in absent {
                ensure!(
                    a["event"]
                        .pointer(path.as_str().context("absent pointer")?)
                        .is_none(),
                    "backend {i} absent field present: {path}"
                );
            }
        }
    }
    Ok(())
}

// Compare the union of declared backend-event fields; never compare arbitrary
// generated fields, and never normalize body bytes or backend target/counts.
fn project(shape: &Value, actual: &Value) -> Value {
    match shape {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        json!({"present":actual.get(k).is_some(),"value":project(v, &actual[k])}),
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(i, v)| project(v, &actual[i]))
                .collect(),
        ),
        _ => actual.clone(),
    }
}
fn call_projection(e: &Value, a: &Value) -> Value {
    Value::Array(
        e["backendCalls"]
            .as_array()
            .unwrap()
            .iter()
            .zip(a["backendCalls"].as_array().unwrap())
            .map(|(e, a)| {
                let mut event = project(&e["event"], &a["event"]);
                if let Some(exact) = e["eventExact"].as_object() {
                    for key in exact.keys() {
                        event[key] = json!({"present":a["event"].get(key).is_some(),"value":a["event"].get(key)});
                    }
                }
                if let Some(absent) = e["eventAbsent"].as_array() {
                    for p in absent {
                        event[format!("absent:{}", p.as_str().unwrap())] =
                            json!(a["event"].pointer(p.as_str().unwrap()).is_none());
                    }
                }
                let mut call = json!({"target":a["target"],"event":event});
                if e.get("eventAuthorizer").is_some() {
                    event["exactAuthorizer"] = a["event"]["requestContext"]["authorizer"].clone();
                }
                if let Some(settings) = e.get("invoke") { call["invoke"] = project(settings, &a["invoke"]); }
                call
            })
            .collect(),
    )
}
fn bound_expected(expected: &Value, actual: &Value) -> Result<Value> {
    let mut bound = expected.clone();
    if expected["httpCalls"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| {
            c["target"]
                .as_str()
                .is_some_and(|s| s.contains("${dependency:http}"))
        })
    {
        let calls = actual["httpCalls"]
            .as_array()
            .context("actual HTTP calls")?;
        let mut origin = None;
        for call in calls {
            let url = url::Url::parse(call["target"].as_str().context("HTTP target")?)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str() == Some("127.0.0.1")
                    && url.port().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none(),
                "dependency target must be a numeric loopback HTTP(S) endpoint"
            );
            let base = url.origin().ascii_serialization();
            if let Some(old) = &origin {
                ensure!(old == &base, "HTTP dependency origin changed within result");
            } else {
                origin = Some(base);
            }
        }
        let base = origin.context("expected HTTP dependency call missing")?;
        crate::application::substitute(&mut bound, &base);
    }
    // A refused endpoint has no received HTTP call from which to bind its
    // owned ephemeral origin. Bind only the declared ERR10089 description
    // template, then retain the ordinary exact-byte body assertion.
    if expected["httpCalls"] == json!([])
        && actual["httpCalls"] == json!([])
        && let Some(body) = expected["body"].as_str()
        && body.contains("${dependency:http}")
    {
        let template: Value = serde_json::from_str(body)?;
        ensure!(
            template["code"] == "ERR10089",
            "unsupported dependency body recipe"
        );
        let description = template["description"]
            .as_str()
            .context("HTTP error description")?;
        let (prefix, suffix) = description
            .split_once("${dependency:http}")
            .context("dependency origin template")?;
        ensure!(
            prefix == "Failed to invoke HTTP service with serviceId "
                && suffix.starts_with('/')
                && suffix.ends_with('.'),
            "unsupported dependency description recipe"
        );
        let observed: Value =
            serde_json::from_str(actual["body"].as_str().context("HTTP error body")?)?;
        let origin = observed["description"]
            .as_str()
            .context("actual description")?
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
            .context("HTTP error description template mismatch")?;
        let url = url::Url::parse(origin)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.origin().ascii_serialization() == origin,
            "refused dependency must use a numeric loopback origin"
        );
        crate::application::substitute(&mut bound, origin);
    }
    Ok(bound)
}

fn http_projection(expected: &Value, actual: &Value) -> Value {
    if expected.get("httpCalls").is_none() {
        return Value::Null;
    }
    let shape = crate::recipes::http_shape(&expected["httpCalls"]);
    let calls = crate::recipes::http_calls(&expected["httpCalls"], &actual["httpCalls"]);
    let mut projected = project(&shape, &calls);
    // Origins are resolved independently for each owned dependency. Only a
    // declared logical dependency target can be canonicalized, after asserting
    // the resolved full URL, method, URI, body and declared headers.
    if let Some(calls) = expected["httpCalls"].as_array() {
        for (i, call) in calls.iter().enumerate() {
            if call["target"]
                .as_str()
                .is_some_and(|s| s.starts_with("${dependency:http}"))
            {
                projected[i]["target"] = serde_json::json!({"present":true,"value":call["target"]});
            }
        }
    }
    projected
}

// Called only after both original per-runner expectations have been asserted,
// including the exact ERR10089 body with its independently bound loopback origin.
// Preserve raw bodies; project only the declared ephemeral-origin template.
fn refusal_body_projection(expected: &Value, actual: &Value) -> Value {
    if expected["httpCalls"] == json!([])
        && actual["httpCalls"] == json!([])
        && expected["body"]
            .as_str()
            .is_some_and(|b| b.contains("${dependency:http}"))
    {
        expected["body"].clone()
    } else {
        actual["body"].clone()
    }
}

pub fn compare(f: &Value, j: &Value, r: &Value) -> Result<()> {
    if crate::router_failed_component::supported(f) {
        ensure!(
            j["execution"] == "passed" && r["execution"] == "passed",
            "actual paired Router component must pass"
        );
        ensure!(
            j["observations"].as_array().is_some_and(|a| a.len() == 1)
                && r["observations"].as_array().is_some_and(|a| a.len() == 1),
            "Router component observation count"
        );
        crate::router_failed_component::assert_observation(&f["expect"], &j["observations"][0])?;
        return crate::router_failed_component::assert_observation(
            &f["expect"],
            &r["observations"][0],
        );
    }
    if crate::single_runner::supported(f) {
        return crate::single_runner::compare(f, j, r);
    }
    let bound = crate::legacy_correlation::bind(f, j)?;
    let f = &bound;
    ensure!(
        f["nondeterministic"].as_array().is_some_and(Vec::is_empty),
        "unimplemented normalization"
    );
    ensure!(
        f["javaOnly"] == false && f["rustOnly"] == false,
        "single-runner adapter not implemented"
    );
    ensure!(
        j["execution"] == "passed" && r["execution"] == "passed",
        "runner did not pass"
    );
    if let Some(expected) = f.get("expectRustInit").or_else(|| f.get("expectInit")) {
        assert_observation(expected, &r["initialization"])?;
    }
    if let Some(expected) = f.get("expectJavaInit").or_else(|| f.get("expectInit")) {
        assert_observation(expected, &j["initialization"])?;
    }
    let cases = crate::application::cases(f);
    let ja = j["observations"].as_array().context("Java observations")?;
    let ra = r["observations"].as_array().context("Rust observations")?;
    ensure!(
        ja.len() == cases.len() && ra.len() == cases.len(),
        "warm observation count differs"
    );
    let mut differences = BTreeSet::new();
    // Explicit Init contracts have their own actual observations. In particular,
    // bootstrap fetches must not disappear merely because per-request fetches
    // are zero on both sides. Trace assertions remain exact per runner.
    if j.get("initialization").is_some() && r.get("initialization").is_some() {
        for key in ["outcome", "backendCalls", "jwksFetches"] {
            if j["initialization"][key] != r["initialization"][key] {
                differences.insert(key.to_owned());
            }
        }
    }
    for (index, (case, (ja, ra))) in cases.iter().zip(ja.iter().zip(ra)).enumerate() {
        let java_expected = crate::authorizer::bind_result(
            case.get("expectJava").unwrap_or(&case["expect"]),
            f,
            j,
            index,
        )?;
        let rust_expected = crate::authorizer::bind_result(&case["expect"], f, r, index)?;
        let je = &java_expected;
        let re = &rust_expected;
        assert_observation_with_window(
            &bound_expected(je, ja)?,
            ja,
            crate::recipes::window(j, index)?,
        )?;
        assert_observation_with_window(
            &bound_expected(re, ra)?,
            ra,
            crate::recipes::window(r, index)?,
        )?;
        for key in ["outcome", "status", "body", "bodyBase64", "jwksFetches"] {
            let different = if key == "body" {
                refusal_body_projection(je, ja) != refusal_body_projection(re, ra)
            } else {
                ja[key] != ra[key]
            };
            if different {
                differences.insert(key.to_owned());
            }
        }
        if (je.get("auditRecords").is_some() || re.get("auditRecords").is_some())
            && (project(&je["auditRecords"], &ja["auditRecords"])
                != project(&je["auditRecords"], &ra["auditRecords"])
                || project(&re["auditRecords"], &ja["auditRecords"])
                    != project(&re["auditRecords"], &ra["auditRecords"]))
        {
            differences.insert("auditRecords".into());
        }
        for key in [
            "configurationWarningObservation",
            "securityLogObservation",
            "responsePolicyObservation",
            "failureProbeResult",
            "boundedWorkProbeExecutions",
            "configurationDiagnostics",
            "backendDiagnostics",
            "ruleFailuresCaught",
            "ruleInputObservation",
            "cacheActions",
            "ldapBinds",
            "traceContext",
            "completionProbeResult",
            "blockingLease",
        ] {
            if (je.get(key).is_some() || re.get(key).is_some()) && ja.get(key) != ra.get(key) {
                differences.insert(key.to_owned());
            }
        }
        if je.get("responses").is_some() || re.get("responses").is_some() {
            let project_responses = |actual: &Value| -> Result<Value> {
                Ok(json!(actual["responses"].as_array().into_iter().flatten().map(|response| -> Result<Value> {
                    Ok(json!({"outcome":response["outcome"],"status":response["status"],"body":response["body"],"bodyBase64":response["bodyBase64"],"headers":header_map(&response["headers"])?}))
                }).collect::<Result<Vec<_>>>()?))
            };
            if project_responses(ja)? != project_responses(ra)? {
                differences.insert("responses".into());
            }
        }
        // Raw nondeterministic timestamp/duration bytes are qualified by each
        // runner's contract; EMF is Rust-only in this closed paired profile.
        if je.get("emf").is_some() != re.get("emf").is_some() {
            differences.insert("emf".into());
        }
        // Timing compares the declared bounded contracts after both actual
        // measurements pass them; elapsed wall times are never exact parity.
        if je.get("invocationTiming") != re.get("invocationTiming") {
            differences.insert("invocationTiming".into());
        }
        let empty = json!([]);
        let jh = header_map(ja.get("headers").unwrap_or(&empty))?;
        let rh = header_map(ra.get("headers").unwrap_or(&empty))?;
        for key in jh.keys().chain(rh.keys()) {
            if jh.get(key) != rh.get(key) {
                differences.insert(format!("header:{key}"));
            }
        }
        let jc = ja["backendCalls"].as_array().context("Java calls")?;
        let rc = ra["backendCalls"].as_array().context("Rust calls")?;
        if jc.len() != rc.len()
            || jc.iter().zip(rc).any(|(j, r)| j["target"] != r["target"])
            || call_projection(je, ja) != call_projection(je, ra)
            || call_projection(re, ja) != call_projection(re, ra)
        {
            differences.insert("backendCalls".into());
        }
        if (je.get("httpCalls").is_some() || re.get("httpCalls").is_some())
            && (ja["httpCalls"].as_array().map_or(0, Vec::len)
                != ra["httpCalls"].as_array().map_or(0, Vec::len)
                || http_projection(je, ja) != http_projection(je, ra)
                || http_projection(re, ja) != http_projection(re, ra))
        {
            differences.insert("httpCalls".into());
        }
    }
    let declared = f["differences"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    if differences != declared {
        return Err(DifferentialMismatch {
            unexpected: differences.difference(&declared).cloned().collect(),
            stale: declared.difference(&differences).cloned().collect(),
            observed: differences,
            declared,
        }
        .into());
    }
    Ok(())
}

/// Retention is opt-in evidence reuse, never a fresh application execution.
fn verify_retained(f: &Value, result: &Value, original: bool) -> Result<()> {
    ensure!(
        result["id"] == f["id"]
            && result["runner"] == "rust"
            && result["execution"] == "passed"
            && result["fixtureDigest"] == crate::fixture_digest(f)?,
        "stale/wrong retained Rust result"
    );
    let effective = if original {
        f.clone()
    } else {
        crate::amendments::effective(f)?
    };
    if crate::router_failed_component::supported(&effective) {
        ensure!(
            result["observations"]
                .as_array()
                .is_some_and(|a| a.len() == 1),
            "retained Router component observation count"
        );
        return crate::router_failed_component::assert_observation(
            &effective["expect"],
            &result["observations"][0],
        );
    }
    if crate::single_runner::supported(&effective) {
        return crate::single_runner::assert_rust(&effective, result);
    }
    if let Some(expected) = effective
        .get("expectRustInit")
        .or_else(|| effective.get("expectInit"))
    {
        assert_observation(expected, &result["initialization"])?;
    }
    let cases = crate::application::cases(&effective);
    let observations = result["observations"]
        .as_array()
        .context("retained observations")?;
    ensure!(
        cases.len() == observations.len(),
        "retained warm observation count"
    );
    for (index, (case, actual)) in cases.iter().zip(observations).enumerate() {
        let expected = crate::authorizer::bind_result(&case["expect"], &effective, result, index)?;
        assert_observation_with_window(
            &bound_expected(&expected, actual)?,
            actual,
            crate::recipes::window(result, index)?,
        )?;
    }
    Ok(())
}
#[cfg(test)]
mod retained_tests {
    use super::*;
    #[test]
    fn approved_component_retention_rechecks_active_version_and_all_jwks_witnesses() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures/v1");
        let f = crate::fixture_files(&root)
            .unwrap()
            .iter()
            .map(|p| crate::load(p).unwrap())
            .find(|f| f["id"] == "S-13c-shared-fetch-panic-recovery-component-v1")
            .unwrap();
        let effective = crate::amendments::effective(&f).unwrap();
        let mut result = json!({"id":f["id"],"runner":"rust","execution":"passed","fixtureDigest":crate::fixture_digest(&f).unwrap(),"observations":[effective["expect"]]});
        verify_retained(&f, &result, false).unwrap();
        assert!(verify_retained(&f, &result, true).is_err());
        result["observations"][0]["recovery"]["jwksPublished"] = json!(false);
        assert!(verify_retained(&f, &result, false).is_err());
    }

    #[tokio::test]
    async fn retained_component_rechecks_actual_stdout_and_rejects_changed_output() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures/v1");
        let f = crate::fixture_files(&root)
            .unwrap()
            .iter()
            .map(|p| crate::load(p).unwrap())
            .find(|f| f["id"] == "CFG-01-default-info-v1-contract-v1")
            .unwrap();
        let mut result = crate::logging_component::run(&f, &"0".repeat(40))
            .await
            .unwrap();
        verify_retained(&f, &result, true).unwrap();
        result["observations"][0]["stdout"] = json!("");
        assert!(verify_retained(&f, &result, true).is_err());
    }
    #[test]
    fn tls_origin_binding_retains_numeric_loopback_and_exact_target_rules() {
        let expected = json!({"httpCalls":[{"target":"${dependency:http}/x"}]});
        let actual = json!({"httpCalls":[{"target":"https://127.0.0.1:43210/x"}]});
        assert_eq!(
            bound_expected(&expected, &actual).unwrap()["httpCalls"],
            actual["httpCalls"]
        );
        for target in [
            "https://example.com:443/x",
            "https://user@127.0.0.1:443/x",
            "https://127.0.0.1/x#fragment",
            "ftp://127.0.0.1:43/x",
        ] {
            assert!(bound_expected(&expected, &json!({"httpCalls":[{"target":target}]})).is_err());
        }
    }
    #[test]
    fn retention_reasserts_contract_and_digest_instead_of_trusting_passed() {
        let observation = json!({"outcome":"initSuccess","backendCalls":[],"jwksFetches":0});
        let f = json!({"id":"probe","expect":observation});
        let mut r = json!({"id":"probe","runner":"rust","execution":"passed","fixtureDigest":crate::fixture_digest(&f).unwrap(),"observations":[observation]});
        verify_retained(&f, &r, true).unwrap();
        r["observations"][0]["jwksFetches"] = json!(1);
        assert!(verify_retained(&f, &r, true).is_err());
        r["observations"][0]["jwksFetches"] = json!(0);
        r["fixtureDigest"] = json!("stale");
        assert!(verify_retained(&f, &r, true).is_err());
    }
}
