//! Static sites derived from raw authored data, before any typed conversion.
use crate::{
    Category, Engine, ExpressionError, Kind, Limits, Position, Profile, Segment, WorkerError,
    resolve_profile, scan,
};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub error: ExpressionError,
    pub task: Option<String>,
    pub field: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    Diagnostic(Diagnostic),
    Worker(WorkerError),
}
struct Site {
    source: String,
    position: Position,
    span: usize,
    offset: usize,
}
enum Finding {
    Site(Site),
    Error(ExpressionError),
}
struct Check {
    field: String,
    task: Option<String>,
    finding: Finding,
}
pub struct DefinitionValidation {
    pub profile: Profile,
    checks: Vec<Check>,
}
fn pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}
impl DefinitionValidation {
    pub fn from_raw(raw: &Value) -> Result<Self, Diagnostic> {
        let profile = resolve_profile(raw).map_err(|error| Diagnostic {
            error,
            task: None,
            field: "/document/metadata/lightExpressionProfile".into(),
        })?;
        let mut plan = Self {
            profile,
            checks: Vec::new(),
        };
        if profile == Profile::LegacyV1 {
            return Ok(plan);
        }
        // Assign preorder ranks with sorted object keys and authored array order.
        // This avoids lexical JSON-pointer sorting incorrectly ordering /10 before /2.
        let mut ranks = HashMap::new();
        let mut todo = vec![(String::new(), raw)];
        while let Some((field, value)) = todo.pop() {
            ranks.insert(field.clone(), ranks.len());
            match value {
                Value::String(s) if s.contains("${{") => {
                    plan.error(&field, None, Category::Unsupported)
                }
                Value::Array(a) => todo.extend(
                    a.iter()
                        .enumerate()
                        .rev()
                        .map(|(i, v)| (pointer(&field, &i.to_string()), v)),
                ),
                Value::Object(o) => {
                    let mut entries: Vec<_> = o.iter().collect();
                    entries.sort_unstable_by_key(|(k, _)| *k);
                    todo.extend(
                        entries
                            .into_iter()
                            .rev()
                            .map(|(k, v)| (pointer(&field, k), v)),
                    );
                }
                _ => (),
            }
        }
        if raw.pointer("/input/from").is_some() {
            plan.error("/input/from", None, Category::Unsupported);
        }
        if raw.pointer("/output/schema").is_some() {
            plan.error("/output/schema", None, Category::Unsupported);
        }
        if let Some(value) = raw.pointer("/output/as") {
            plan.json(value, "/output/as", None, Position::WorkflowOutput, true, 0);
        }
        plan.tasks(raw.get("do"), "/do", false);
        for check in &mut plan.checks {
            if check.task.is_none() && check.field.starts_with("/do/") {
                // Task path is /do/index/name, also for nested fork branches.
                // Derive identity from the raw path, never from runtime values.
                let parts: Vec<_> = check.field.split('/').collect();
                check.task = parts
                    .get(3)
                    .map(|name| name.replace("~1", "/").replace("~0", "~"));
                if let Some(index) = parts
                    .windows(2)
                    .rposition(|pair| pair == ["fork", "branches"])
                {
                    check.task = parts
                        .get(index + 3)
                        .map(|name| name.replace("~1", "/").replace("~0", "~"));
                }
            }
        }
        plan.checks.sort_by_key(|check| {
            (
                ranks.get(&check.field).copied().unwrap_or(usize::MAX),
                check.field.clone(),
                match &check.finding {
                    Finding::Error(_) => 0,
                    Finding::Site(site) => site.span + 1,
                },
            )
        });
        Ok(plan)
    }
    pub async fn validate(&self, engine: &Engine) -> Result<(), ValidationError> {
        for check in &self.checks {
            let error = match &check.finding {
                Finding::Error(error) => Some(error.clone()),
                Finding::Site(site) => {
                    let compilation = engine
                        .enqueue(
                            self.profile,
                            site.source.clone(),
                            site.position,
                            Limits::default(),
                        )
                        .map_err(ValidationError::Worker)?;
                    match compilation.receive().await {
                        Ok(handle) => {
                            drop(handle);
                            None
                        }
                        Err(WorkerError::Expression(error)) => {
                            Some(error.at(site.span, site.offset))
                        }
                        Err(error) => return Err(ValidationError::Worker(error)),
                    }
                }
            };
            if let Some(error) = error {
                return Err(ValidationError::Diagnostic(Diagnostic {
                    error,
                    field: check.field.clone(),
                    task: check.task.clone(),
                }));
            }
        }
        Ok(())
    }
    fn error(&mut self, field: &str, task: Option<&str>, category: Category) {
        self.checks.push(Check {
            field: field.into(),
            task: task.map(str::to_owned),
            finding: Finding::Error(crate::error::admission(category)),
        });
    }
    fn text(
        &mut self,
        value: &Value,
        field: &str,
        task: Option<&str>,
        position: Position,
        required: bool,
    ) {
        let Some(text) = value.as_str() else {
            self.error(field, task, Category::Invalid);
            return;
        };
        let segments = match scan(text) {
            Ok(segments) => segments,
            Err(error) => {
                self.checks.push(Check {
                    field: field.into(),
                    task: task.map(str::to_owned),
                    finding: Finding::Error(error),
                });
                return;
            }
        };
        let whole =
            matches!(segments.as_slice(),[Segment::Span {offset:0,end,..}] if *end==text.len());
        if required
            && (matches!(position.kind(), Kind::Predicate | Kind::Export)
                || position == Position::WorkflowOutput)
            && !whole
        {
            self.error(field, task, Category::Invalid);
            return;
        }
        for segment in segments {
            if let Segment::Span {
                source,
                source_offset,
                index,
                ..
            } = segment
            {
                self.checks.push(Check {
                    field: field.into(),
                    task: task.map(str::to_owned),
                    finding: Finding::Site(Site {
                        source,
                        position,
                        span: index,
                        offset: source_offset,
                    }),
                });
            }
        }
    }
    fn json(
        &mut self,
        value: &Value,
        field: &str,
        task: Option<&str>,
        position: Position,
        root: bool,
        depth: usize,
    ) {
        if depth > Limits::default().output_depth {
            self.error(field, task, Category::Limit);
            return;
        }
        if root && position == Position::WorkflowOutput && !value.is_object() && !value.is_string()
        {
            self.error(field, task, Category::Invalid);
            return;
        }
        match value {
            Value::String(_) => self.text(
                value,
                field,
                task,
                if position == Position::WorkflowOutput && !root {
                    Position::Set
                } else {
                    position
                },
                root,
            ),
            Value::Object(o) => {
                let mut entries: Vec<_> = o.iter().collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                for (k, v) in entries {
                    self.json(v, &pointer(field, k), task, position, false, depth + 1);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    self.json(
                        v,
                        &pointer(field, &i.to_string()),
                        task,
                        position,
                        false,
                        depth + 1,
                    );
                }
            }
            _ if position.kind() == Kind::String || position.kind() == Kind::Predicate => {
                self.error(field, task, Category::Invalid)
            }
            _ => (),
        }
    }
    fn map_strings(&mut self, value: &Value, field: &str, task: Option<&str>, position: Position) {
        let Some(map) = value.as_object() else {
            self.error(field, task, Category::Invalid);
            return;
        };
        for (key, value) in map {
            self.text(value, &pointer(field, key), task, position, true);
        }
    }
    fn rpc_headers(&mut self, value: &Value, field: &str, task: Option<&str>, position: Position) {
        if value.is_string() {
            if value.as_str().is_some_and(|text| !matches!(scan(text).ok().as_deref(),Some([Segment::Span{offset:0,end,..}]) if *end==text.len())) {
                self.error(field,task,Category::Invalid);
                return;
            }
            self.text(value, field, task, position, true);
        } else if let Some(map) = value.as_object() {
            for (key, value) in map {
                self.text(value, &pointer(field, key), task, position, true);
            }
        } else {
            self.error(field, task, Category::Invalid);
        }
    }
    fn endpoint(
        &mut self,
        value: &Value,
        field: &str,
        task: Option<&str>,
        position: Option<Position>,
    ) {
        let (value, field) = if value.is_string() {
            (value, field.to_owned())
        } else {
            let Some(uri) = value.get("uri") else {
                self.error(field, task, Category::Invalid);
                return;
            };
            (uri, pointer(field, "uri"))
        };
        if let Some(position) = position {
            self.text(value, &field, task, position, true);
        } else if value
            .as_str()
            .is_none_or(|uri| crate::uri::validate_path_placeholders(uri).is_err())
        {
            self.error(&field, task, Category::Invalid);
        }
    }
    fn tasks(&mut self, value: Option<&Value>, field: &str, inside_fork: bool) {
        let Some(array) = value.and_then(Value::as_array) else {
            self.error(field, None, Category::Invalid);
            return;
        };
        for (i, entry) in array.iter().enumerate() {
            let path = pointer(field, &i.to_string());
            let Some(map) = entry.as_object().filter(|map| map.len() == 1) else {
                self.error(&path, None, Category::Invalid);
                continue;
            };
            let (name, task) = map.iter().next().unwrap();
            let path = pointer(&path, name);
            self.task(task, &path, name, inside_fork);
        }
    }
    fn task(&mut self, value: &Value, field: &str, name: &str, inside_fork: bool) {
        let Some(task) = value.as_object() else {
            self.error(field, Some(name), Category::Invalid);
            return;
        };
        for unsupported in ["if", "input", "output"] {
            if task.contains_key(unsupported) {
                self.error(
                    &pointer(field, unsupported),
                    Some(name),
                    Category::Unsupported,
                );
            }
        }
        if inside_fork {
            for key in ["then", "export"] {
                if task.contains_key(key) {
                    self.error(&pointer(field, key), Some(name), Category::Unsupported);
                }
            }
        }
        for unsupported in ["agentTask", "do", "emit", "for", "listen", "raise", "try"] {
            if task.contains_key(unsupported) {
                self.error(
                    &pointer(field, unsupported),
                    Some(name),
                    Category::Unsupported,
                );
            }
        }
        if let Some(export) = task.get("export") {
            let path = pointer(&pointer(field, "export"), "as");
            if let Some(map) = export.get("as").and_then(Value::as_object) {
                for (key, value) in map {
                    self.json(
                        value,
                        &pointer(&path, key),
                        Some(name),
                        if task.contains_key("run") {
                            Position::RunnerExport
                        } else {
                            Position::Export
                        },
                        true,
                        0,
                    );
                }
            } else {
                self.error(&path, Some(name), Category::Unsupported);
            }
        }
        if let Some(value) = task.get("set") {
            self.json(
                value,
                &pointer(field, "set"),
                Some(name),
                Position::Set,
                true,
                0,
            );
        }
        if let Some(switch) = task.get("switch") {
            let path = pointer(field, "switch");
            if let Some(cases) = switch.as_array() {
                let mut default = false;
                for (i, case) in cases.iter().enumerate() {
                    let path = pointer(&path, &i.to_string());
                    let Some(map) = case.as_object().filter(|m| m.len() == 1) else {
                        self.error(&path, Some(name), Category::Invalid);
                        continue;
                    };
                    let (case_name, case) = map.iter().next().unwrap();
                    let path = pointer(&path, case_name);
                    if !case.is_object() || case.get("then").and_then(Value::as_str).is_none() {
                        self.error(&path, Some(name), Category::Invalid);
                    }
                    if case_name == "default" {
                        if default || i + 1 != cases.len() || case.get("when").is_some() {
                            self.error(&path, Some(name), Category::Invalid);
                        }
                        default = true;
                    } else if let Some(when) = case.get("when") {
                        self.text(
                            when,
                            &pointer(&path, "when"),
                            Some(name),
                            Position::SwitchWhen,
                            true,
                        );
                    } else {
                        self.error(&path, Some(name), Category::Invalid);
                    }
                }
            } else {
                self.error(&path, Some(name), Category::Invalid);
            }
        }
        if let Some(assert) = task.get("assert") {
            let path = pointer(field, "assert");
            for key in ["schema", "rule"] {
                if assert.get(key).is_some() {
                    self.error(&pointer(&path, key), Some(name), Category::Unsupported);
                }
            }
            for (key, position) in [
                ("value", Position::AssertValue),
                ("equals", Position::AssertEquals),
                ("contains", Position::AssertContains),
            ] {
                if let Some(value) = assert.get(key) {
                    self.json(value, &pointer(&path, key), Some(name), position, true, 0);
                }
            }
            if let Some(json) = assert.get("json").and_then(Value::as_object) {
                for (key, comparison) in json {
                    let path = pointer(&pointer(&path, "json"), key);
                    if comparison.is_string() {
                        self.text(
                            comparison,
                            &path,
                            Some(name),
                            Position::AssertJsonPredicate,
                            true,
                        );
                    } else {
                        for (key, position) in [
                            ("equals", Position::AssertJsonEquals),
                            ("contains", Position::AssertJsonContains),
                        ] {
                            if let Some(value) = comparison.get(key) {
                                self.json(
                                    value,
                                    &pointer(&path, key),
                                    Some(name),
                                    position,
                                    true,
                                    0,
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Some(assignment) = task.get("ask").and_then(|v| v.get("assignment")) {
            for (key, position) in [
                ("categoryCode", Position::AskCategory),
                ("reasonCode", Position::AskReason),
                ("assigneeId", Position::AskAssignee),
                ("roleId", Position::AskRole),
            ] {
                if let Some(value) = assignment.get(key) {
                    self.text(
                        value,
                        &pointer(&pointer(&pointer(field, "ask"), "assignment"), key),
                        Some(name),
                        position,
                        true,
                    );
                }
            }
        }
        if let Some(fork) = task.get("fork") {
            let path = pointer(field, "fork");
            if inside_fork {
                self.error(&path, Some(name), Category::Unsupported);
            } else {
                self.tasks(fork.get("branches"), &pointer(&path, "branches"), true);
            }
        }
        if let Some(run) = task.get("run")
            && run.get("workflow").is_some()
        {
            self.error(
                &pointer(&pointer(field, "run"), "workflow"),
                Some(name),
                Category::Unsupported,
            );
        }
        let Some(call) = task.get("call") else { return };
        let call = call.as_str().unwrap_or("");
        if !["http", "jsonrpc", "openrpc", "mcp", "a2a", "agent"].contains(&call) {
            self.error(&pointer(field, "call"), Some(name), Category::Unsupported);
            return;
        }
        let Some(args) = task.get("with").and_then(Value::as_object) else {
            self.error(&pointer(field, "with"), Some(name), Category::Invalid);
            return;
        };
        let path = pointer(field, "with");
        if ["http", "mcp", "a2a"].contains(&call)
            && let Some(value) = task.get("idempotencyKey")
        {
            self.text(
                value,
                &pointer(field, "idempotencyKey"),
                Some(name),
                Position::IdempotencyKey,
                true,
            );
        }
        let positions: &[(&str, Position)] = match call {
            "http" => &[("body", Position::HttpBody)],
            "jsonrpc" | "openrpc" => &[("params", Position::JsonRpcParams)],
            "mcp" => &[
                ("params", Position::McpParams),
                ("parameters", Position::McpParams),
                ("arguments", Position::McpParams),
            ],
            "a2a" => &[("parameters", Position::A2aParameters)],
            "agent" => &[
                ("input", Position::AgentInput),
                ("mockOutput", Position::AgentMockOutput),
            ],
            _ => &[],
        };
        for (key, position) in positions {
            if let Some(value) = args.get(*key) {
                self.json(value, &pointer(&path, key), Some(name), *position, true, 0);
            }
        }
        match call {
            "http" => {
                if args.contains_key("output") {
                    self.error(&pointer(&path, "output"), Some(name), Category::Unsupported);
                }
                if let Some(value) = args.get("endpoint") {
                    self.endpoint(value, &pointer(&path, "endpoint"), Some(name), None);
                }
                for (key, position) in [
                    ("headers", Position::HttpHeader),
                    ("query", Position::HttpQuery),
                ] {
                    if let Some(value) = args.get(key) {
                        self.map_strings(value, &pointer(&path, key), Some(name), position);
                    }
                }
            }
            "jsonrpc" => {
                if let Some(value) = args.get("endpoint") {
                    self.endpoint(
                        value,
                        &pointer(&path, "endpoint"),
                        Some(name),
                        Some(Position::JsonRpcUri),
                    );
                }
                if let Some(value) = args.get("headers") {
                    self.rpc_headers(
                        value,
                        &pointer(&path, "headers"),
                        Some(name),
                        Position::JsonRpcHeader,
                    );
                }
            }
            "openrpc" => {
                if let Some(headers) = args.get("headers") {
                    self.rpc_headers(
                        headers,
                        &pointer(&path, "headers"),
                        Some(name),
                        Position::JsonRpcHeader,
                    );
                }
                if let Some(endpoint) = args.get("document").and_then(|d| d.get("endpoint")) {
                    self.endpoint(
                        endpoint,
                        &pointer(&pointer(&path, "document"), "endpoint"),
                        Some(name),
                        Some(Position::OpenRpcEndpoint),
                    );
                }
                if let Some(server) = args.get("server") {
                    if server.is_string() {
                        self.text(
                            server,
                            &pointer(&path, "server"),
                            Some(name),
                            Position::JsonRpcUri,
                            true,
                        );
                    } else if let Some(url) = server.get("url") {
                        self.text(
                            url,
                            &pointer(&pointer(&path, "server"), "url"),
                            Some(name),
                            Position::JsonRpcUri,
                            true,
                        );
                    }
                }
            }
            "mcp" => {
                if args.get("transport").and_then(|v| v.get("stdio")).is_some() {
                    self.error(
                        &pointer(&pointer(&path, "transport"), "stdio"),
                        Some(name),
                        Category::Unsupported,
                    );
                }
                if let Some(value) = args.get("resource") {
                    self.text(
                        value,
                        &pointer(&path, "resource"),
                        Some(name),
                        Position::McpResourceUri,
                        true,
                    );
                }
            }
            "agent" => {
                for (key, position) in [
                    ("instructions", Position::AgentInstructions),
                    ("prompt", Position::AgentPrompt),
                ] {
                    if let Some(value) = args.get(key) {
                        self.text(value, &pointer(&path, key), Some(name), position, true);
                    }
                }
            }
            _ => (),
        }
    }
}
