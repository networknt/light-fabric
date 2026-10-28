//! Owner-approved finite Transformer program profile. Factory admission is separate.
use async_trait::async_trait;
use light_rule::{ActionRegistry, EndpointConfig, Rule, RuleActionPlugin, RuleConfig, RuleEngine};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    sync::{Arc, LazyLock},
};
use tokio::{sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;
pub const FILE_BYTES: usize = 65536;
pub const CONTEXT_BYTES: usize = 32768;
pub const RULES: usize = 32;
pub const ACTIONS_PER_RULE: usize = 4;
pub const LITERAL_BYTES: usize = 4096;
pub const WORKER_COUNT: usize = 2;
pub const DEPTH: usize = 16;
pub const CONTEXT_NODES: usize = 1024;
pub const OCCURRENCES: usize = 32;
fn string_bytes(s: &str) -> usize {
    2 + s
        .chars()
        .map(|c| match c {
            '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
            c if (c as u32) < 32 => 6,
            c => c.len_utf8(),
        })
        .sum::<usize>()
}
// Count before serialization or cloning. The source Value remains borrowed.
pub fn tree(value: &Value, bytes: usize, nodes: usize) -> Result<(), String> {
    let mut pending = vec![(value, 0)];
    let mut seen = 0;
    let mut used = 0usize;
    while let Some((value, depth)) = pending.pop() {
        seen += 1;
        if seen > nodes || depth > DEPTH {
            return Err(rejected());
        }
        used = used
            .checked_add(match value {
                Value::Null => 4,
                Value::Bool(true) => 4,
                Value::Bool(false) => 5,
                Value::Number(n) => n.to_string().len(),
                Value::String(s) => {
                    if s.len() > bytes {
                        return Err(rejected());
                    }
                    string_bytes(s)
                }
                Value::Array(a) => {
                    if a.len() > 256 {
                        return Err(rejected());
                    }
                    pending.extend(a.iter().map(|v| (v, depth + 1)));
                    2 + a.len().saturating_sub(1)
                }
                Value::Object(m) => {
                    if m.len() > 64 || m.keys().any(|k| k.len() > 256) {
                        return Err(rejected());
                    }
                    pending.extend(m.values().map(|v| (v, depth + 1)));
                    2 + m.len().saturating_sub(1)
                        + m.keys().map(|k| string_bytes(k) + 1).sum::<usize>()
                }
            })
            .ok_or_else(rejected)?;
        if used > bytes {
            return Err(rejected());
        }
    }
    Ok(())
}
pub fn context(value: &Value) -> Result<(), String> {
    if !value.is_object() {
        return Err(rejected());
    }
    tree(value, CONTEXT_BYTES, CONTEXT_NODES)
}
static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(WORKER_COUNT)));
#[derive(Clone, Copy)]
enum Action {
    RequestBody,
    ResponseBody,
    Metadata,
    LocalResponse,
    ValidationError,
    ResponseHeaders,
}
const ACTIONS: [(&str, Action); 6] = [
    ("request-body-literal", Action::RequestBody),
    ("response-body-literal", Action::ResponseBody),
    ("request-metadata-literal", Action::Metadata),
    ("request-local-response-literal", Action::LocalResponse),
    ("request-validation-error-literal", Action::ValidationError),
    ("response-headers-literal", Action::ResponseHeaders),
];
fn rejected() -> String {
    "candidate rule profile rejected".into()
}
fn headers(value: &Value) -> Result<(), String> {
    let m = value.as_object().ok_or_else(rejected)?;
    if m.keys().any(|k| k != "remove" && k != "update") {
        return Err(rejected());
    }
    if let Some(a) = m.get("remove") {
        let a = a.as_array().ok_or_else(rejected)?;
        if a.len() > 32
            || a.iter().any(|v| {
                !v.as_str().is_some_and(|s| {
                    s.len() <= 128 && http::HeaderName::from_bytes(s.as_bytes()).is_ok()
                })
            })
        {
            return Err(rejected());
        }
    }
    if let Some(a) = m.get("update") {
        let a = a.as_object().ok_or_else(rejected)?;
        if a.len() > 32
            || a.iter().any(|(k, v)| {
                k.len() > 128
                    || http::HeaderName::from_bytes(k.as_bytes()).is_err()
                    || !v
                        .as_str()
                        .is_some_and(|s| s.len() <= 1024 && http::HeaderValue::from_str(s).is_ok())
            })
        {
            return Err(rejected());
        }
    }
    Ok(())
}
fn values(action: Action, value: &Value) -> Result<(), String> {
    tree(value, LITERAL_BYTES, CONTEXT_NODES)?;
    let keys = match action {
        Action::RequestBody | Action::ResponseBody => vec!["body"],
        Action::Metadata => vec![
            "requestPath",
            "queryString",
            "requestHeaders",
            "requestBody",
        ],
        Action::LocalResponse => vec!["responseBody", "statusCode"],
        Action::ValidationError => vec!["validationError", "errorMessage"],
        Action::ResponseHeaders => vec!["remove", "update"],
    };
    let m = value.as_object().ok_or_else(rejected)?;
    if m.keys().any(|k| !keys.contains(&k.as_str())) {
        return Err(rejected());
    }
    if matches!(action, Action::ResponseHeaders) {
        return headers(value);
    }
    for (k, v) in m {
        match k.as_str() {
            "requestHeaders" => headers(v)?,
            "statusCode" => {
                if !v.as_u64().is_some_and(|n| (100..=599).contains(&n)) {
                    return Err(rejected());
                }
            }
            "validationError" => {
                if !v.is_boolean() {
                    return Err(rejected());
                }
            }
            _ => {
                if !v.is_string() {
                    return Err(rejected());
                }
            }
        }
    }
    // These strings are immutable rule literals. Reject malformed payloads at
    // Init rather than turning a static configuration defect into a request500.
    for key in ["queryString", "errorMessage"] {
        if let Some(raw) = m.get(key) {
            let parsed: Value =
                serde_json::from_str(raw.as_str().ok_or_else(rejected)?).map_err(|_| rejected())?;
            tree(&parsed, LITERAL_BYTES, CONTEXT_NODES)?;
            let parsed = parsed.as_object().ok_or_else(rejected)?;
            if key == "queryString" && parsed.values().any(Value::is_null) {
                return Err(rejected());
            }
            if key == "errorMessage"
                && (!parsed
                    .get("statusCode")
                    .and_then(Value::as_u64)
                    .is_some_and(|n| (100..=599).contains(&n))
                    || ["code", "message", "description", "severity"]
                        .iter()
                        .any(|k| !parsed.get(*k).is_some_and(Value::is_string))
                    || parsed.get("metadata").is_some_and(|v| !v.is_object()))
            {
                return Err(rejected());
            }
        }
    }
    Ok(())
}
#[async_trait]
impl RuleActionPlugin for Action {
    async fn execute(
        &self,
        context: &mut Value,
        raw: &Option<Value>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let v = raw.as_ref().ok_or("literal action values missing")?;
        values(*self, v)?;
        self::context(context)?;
        match self {
            Action::RequestBody => context["requestBody"] = v["body"].clone(),
            Action::ResponseBody => context["responseBody"] = v["body"].clone(),
            Action::Metadata | Action::LocalResponse | Action::ValidationError => {
                for (k, v) in v.as_object().ok_or("literal object required")? {
                    context[k] = v.clone();
                }
            }
            Action::ResponseHeaders => context["responseHeaders"] = v.clone(),
        }
        // Each mutation copies one <=4096-byte literal plus fixed field names.
        // A new responseHeaders wrapper adds19 serialized bytes. Validate every
        // intermediate, not merely the result after all actions in a rule.
        self::context(context)?;
        Ok(true)
    }
}
pub struct Candidate {
    compile_bytes: usize,
    engine: Arc<RuleEngine>,
    rules: BTreeMap<String, Rule>,
    endpoints: HashMap<String, EndpointConfig>,
}
impl Candidate {
    pub fn compile(raw: &Value) -> Result<Self, String> {
        tree(raw, FILE_BYTES, 16384)?;
        // Check raw declarations before serde's Java alias can erase provenance.
        for rule in raw["ruleBodies"].as_object().ok_or_else(rejected)?.values() {
            for action in rule["actions"].as_array().ok_or_else(rejected)? {
                if action.get("actionClassName").is_some() {
                    return Err("Java class plugin".into());
                }
            }
        }
        let config: RuleConfig = serde_json::from_value(raw.clone()).map_err(|_| rejected())?;
        if config.rule_bodies.len() > RULES {
            return Err(rejected());
        }
        let mut registry = ActionRegistry::new();
        for (name, action) in ACTIONS {
            registry.register(name, Arc::new(action));
        }
        let engine = Arc::new(RuleEngine::new(Arc::new(registry)));
        let rules = config.rule_bodies.into_iter().collect::<BTreeMap<_, _>>();
        if config.endpoint_rules.len() > 64 {
            return Err(rejected());
        }
        for (endpoint, EndpointConfig::Map(phases)) in &config.endpoint_rules {
            if endpoint.len() > 256 || endpoint.contains(['{', '*']) {
                return Err(rejected());
            }
            for (phase, names) in phases {
                if !matches!(phase.as_str(), "req-tra" | "res-tra") {
                    return Err(rejected());
                }
                let names = names.as_array().ok_or_else(rejected)?;
                if names.len() > RULES
                    || names.iter().any(|n| {
                        !n.as_str()
                            .is_some_and(|n| rules.get(n).is_some_and(|r| r.rule_type == *phase))
                    })
                {
                    return Err(rejected());
                }
            }
        }
        let equality =
            regex::Regex::new(r"^(requestBody|requestPath) == '[A-Za-z0-9 _/-]{0,256}'$")
                .map_err(|error| format!("candidate CEL preparation: {error}"))?;
        for rule in rules.values() {
            let expression = rule.expression.as_deref().ok_or_else(rejected)?;
            // A finite grammar: constants or a single bounded literal equality.
            if expression.len() > 512
                || !(expression == "true"
                    || expression == "false"
                    || equality.is_match(expression)
                    || expression == "int(requestHeaders['test-rule']) > 0")
                || rule.condition_language.as_deref() != Some("cel")
                || rule.conditions.as_ref().is_some_and(|c| !c.is_empty())
                || !matches!(rule.rule_type.as_str(), "req-tra" | "res-tra")
                || rule.condition_security_profile.as_deref() != Some("standard")
            {
                return Err(rejected());
            }
            let actions = rule.actions.as_ref().ok_or_else(rejected)?;
            if actions.len() > ACTIONS_PER_RULE {
                return Err(rejected());
            }
            for a in actions {
                let action = ACTIONS
                    .iter()
                    .find(|(n, _)| *n == a.action_ref)
                    .ok_or("missing enforceable action bound")?
                    .1;
                let response = rule.rule_type == "res-tra";
                if response != matches!(action, Action::ResponseBody | Action::ResponseHeaders) {
                    return Err(rejected());
                }
                values(action, a.action_values.as_ref().ok_or_else(rejected)?)?;
            }
            // Warm the actual CEL compile/cache during candidate preparation;
            // do not execute actions or install a different evaluator.
            let warm = engine.evaluate_cel_predicate(
                &rule.rule_id,
                expression,
                Some("standard"),
                &rule.rule_type,
                &json!({"requestBody":"","requestPath":"","requestHeaders":{"test-rule":"1"}}),
            );
            if expression == "int(requestHeaders['test-rule']) > 0" {
                // Response CEL deliberately lacks requestHeaders; request CEL
                // can read it. Both exact fixture expressions are parsed and
                // cached above. Their evaluation errors are caught at runtime.
                let _ = warm;
            } else {
                warm.map_err(|error| format!("candidate CEL warm: {error}"))?;
            }
        }
        Ok(Self {
            compile_bytes: raw.to_string().len(),
            engine,
            rules,
            endpoints: config.endpoint_rules,
        })
    }
    pub fn execute(
        &self,
        mut context: Value,
        endpoint: &str,
        phase: &str,
    ) -> Result<Value, String> {
        self::context(&context)?;
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            let Some(EndpointConfig::Map(phases)) = self.endpoints.get(endpoint) else {
                return Ok(context);
            };
            let Some(names) = phases.get(phase) else {
                return Ok(context);
            };
            for name in names.as_array().ok_or_else(rejected)? {
                let rule = self
                    .rules
                    .get(name.as_str().ok_or_else(rejected)?)
                    .ok_or_else(rejected)?;
                // The fixed conversion/index condition can fail at runtime.
                // Preserve the approved caught-engine-error continuation: stop
                // this Transformer, retain the pre-action context, no error body.
                let before = context.clone();
                let outcome = self.engine.execute_rule(rule, &mut context).await;
                if outcome.is_err() {
                    let outside_profile = self::context(&context).is_err();
                    context = before;
                    if outside_profile {
                        return Err("candidate transformed output restriction".into());
                    }
                    break;
                }
                if !outcome.expect("checked rule outcome") {
                    break;
                }
                self::context(&context)?;
            }
            Ok(context)
        })
    }
}
pub fn permits() -> usize {
    WORKERS.available_permits()
}
pub async fn worker<F>(
    input: Value,
    deadline: Instant,
    cancel: CancellationToken,
    job: F,
) -> Result<Value, String>
where
    F: FnOnce(Value) -> Result<Value, String> + Send + 'static,
{
    if cancel.is_cancelled() || Instant::now() >= deadline {
        return Err("deadline".into());
    }
    context(&input)?;
    let permit = tokio::select! {biased;_=cancel.cancelled()=>return Err("deadline".into()),_=tokio::time::sleep_until(deadline)=>return Err("deadline".into()),p=WORKERS.clone().acquire_owned()=>p.map_err(|_|rejected())?};
    if cancel.is_cancelled() || Instant::now() >= deadline {
        return Err("deadline".into());
    }
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        job(input)
    });
    let result = tokio::select! {biased;_=cancel.cancelled()=>return Err("deadline".into()),_=tokio::time::sleep_until(deadline)=>return Err("deadline".into()),r=task=>r.map_err(|_|"worker panic".to_owned())??};
    if cancel.is_cancelled() || Instant::now() >= deadline {
        return Err("deadline".into());
    }
    Ok(result)
}

/// Combined request/response/repeated-occurrence admission over shared programs.
pub fn aggregate(occurrences: &[(&Candidate, &str)]) -> Result<(), String> {
    if occurrences.len() > OCCURRENCES {
        return Err(rejected());
    }
    let mut compiled = std::collections::BTreeSet::new();
    let (
        mut compile_rules,
        mut compile_actions,
        mut compile_bytes,
        mut eval_rules,
        mut eval_actions,
    ) = (0, 0, 0, 0, 0);
    for (candidate, phase) in occurrences {
        if !matches!(*phase, "req-tra" | "res-tra") {
            return Err(rejected());
        }
        if compiled.insert(std::ptr::from_ref(*candidate) as usize) {
            compile_rules += candidate.rules.len();
            compile_bytes += candidate.compile_bytes;
            compile_actions += candidate
                .rules
                .values()
                .map(|r| r.actions.as_ref().map_or(0, Vec::len))
                .sum::<usize>();
        }
        let mut rules = 0;
        let mut actions = 0;
        for EndpointConfig::Map(phases) in candidate.endpoints.values() {
            if let Some(names) = phases.get(*phase) {
                let names = names.as_array().ok_or_else(rejected)?;
                rules = rules.max(names.len());
                actions = actions.max(
                    names
                        .iter()
                        .map(|n| {
                            candidate.rules[n.as_str().expect("compiled rule name")]
                                .actions
                                .as_ref()
                                .map_or(0, Vec::len)
                        })
                        .sum(),
                );
            }
        }
        eval_rules += rules;
        eval_actions += actions;
        if compile_bytes > FILE_BYTES
            || compile_rules > RULES
            || compile_actions > RULES * ACTIONS_PER_RULE
            || eval_rules > RULES
            || eval_actions > RULES * ACTIONS_PER_RULE
        {
            return Err(rejected());
        }
    }
    Ok(())
}
