use crate::{
    compat::{self, Compatibility, Disposition, Phase},
    *,
};
use http::Method;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    Malformed(&'static str),
    UnknownClass(String),
    UnknownAlias(String),
    Cycle(String),
    Deferred(HandlerId),
    Unimplemented(HandlerId),
    DuplicateRoute(String),
    MissingTerminal,
    InvalidTerminal,
    IdentityOrdering,
    FactoryContract(HandlerId),
}
impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InvalidConfiguration: {self:?}")
    }
}
impl std::error::Error for InitError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditConfig {
    pub audit_on_error: bool,
    pub audit_stack_trace: bool,
}

/// Factories must derive these capabilities from effective policy configuration,
/// not alias spelling. Conditional/skip-path authentication is NOT an unconditional
/// identity guarantee. WP9a/WP12 own that policy analysis; WP7 uses test factories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCapabilities {
    pub establishes_verified_identity: bool,
    pub requires_verified_identity: bool,
}
impl RequestCapabilities {
    /// Minimum ordering classification, not a limiter implementation. WP12 still
    /// validates the resolver allowlist and other limit.yml settings.
    pub fn for_limit(key: &str, resolver: Option<&str>) -> Result<Self, InitError> {
        let key = key.to_ascii_lowercase();
        if !["server", "instance", "address", "client", "user"].contains(&key.as_str())
            && !key.starts_with("jwt")
        {
            return Err(InitError::Malformed("limit key"));
        }
        let jwt_resolver = resolver
            .and_then(|r| r.rsplit('.').next())
            .is_some_and(|r| r.to_ascii_lowercase().starts_with("jwt"));
        Ok(Self {
            establishes_verified_identity: false,
            requires_verified_identity: key == "client"
                || key == "user"
                || key.starts_with("jwt")
                || jwt_resolver,
        })
    }
}
#[derive(Clone)]
pub enum Binding {
    Request(Arc<dyn RequestHandler>, RequestCapabilities),
    Response(Arc<dyn ResponseHandler>),
    Dispatch(TargetRoutes),
    Local(Arc<dyn RequestHandler>),
}
/// Trusted factory facts; default preserves legacy, non-consuming chains.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum ValidatorMediaRole {
    #[default]
    Unspecified,
    Preserves,
    Maintains,
    Requires,
}
pub trait HandlerFactory: Send + Sync {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        ValidatorMediaRole::Unspecified
    }
    fn build(&self, definition: &Compatibility, alias: &str) -> Result<Binding, InitError>;
}
enum Registration {
    Disabled,
    Factory(Arc<dyn HandlerFactory>),
}
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<HandlerId, Registration>,
}
impl Registry {
    pub fn disable(&mut self, id: HandlerId) {
        self.entries.insert(id, Registration::Disabled);
    }
    pub fn register(&mut self, id: HandlerId, factory: Arc<dyn HandlerFactory>) {
        self.entries.insert(id, Registration::Factory(factory));
    }
    fn build(
        &self,
        c: &Compatibility,
        alias: &str,
    ) -> Result<Option<(Binding, ValidatorMediaRole)>, InitError> {
        if matches!(self.entries.get(&c.id), Some(Registration::Disabled)) {
            return Ok(None);
        }
        if c.disposition == Disposition::Deferred {
            return Err(InitError::Deferred(c.id));
        }
        let Some(Registration::Factory(factory)) = self.entries.get(&c.id) else {
            return Err(InitError::Unimplemented(c.id));
        };
        let binding = factory.build(c, alias)?;
        let valid = match (&binding, c.phase) {
            (Binding::Request(h, caps), Phase::Request) => {
                h.id() == c.id
                    && (!caps.establishes_verified_identity
                        || matches!(
                            c.id,
                            HandlerId::Security
                                | HandlerId::Jwt
                                | HandlerId::Basic
                                | HandlerId::ApiKey
                        ))
                    && (!caps.requires_verified_identity || c.id == HandlerId::Limit)
            }
            (Binding::Response(h), Phase::Response) => h.id() == c.id,
            (Binding::Local(h), Phase::Local) => h.id() == c.id,
            (Binding::Dispatch(_), Phase::Dispatch) => true,
            _ => false,
        };
        if !valid {
            return Err(InitError::FactoryContract(c.id));
        }
        if c.id == HandlerId::Audit
            && let Binding::Request(handler, _) = &binding
            && handler.is_disabled_audit()
        {
            return Ok(None);
        }
        Ok(Some((binding, factory.validator_media_role())))
    }
}

#[derive(Clone, Debug)]
enum Segment {
    Literal(String),
    Parameter(String),
    Wildcard,
}
#[derive(Clone, Debug)]
struct Template {
    text: String,
    segments: Vec<Segment>,
}
/// Immutable compiler-owned selector language. No ingress constructor or
/// serialization: authorization refinements may only use configured facts.
#[derive(Default)]
pub struct OperationSelection {
    template: Option<Template>,
    method: Option<Method>,
}
impl OperationSelection {
    fn from_configured_route(path: &str, verb: &str) -> Self {
        match (Template::parse(path), method(verb)) {
            (Ok(template), Ok(method)) => Self {
                template: Some(template),
                method: Some(method),
            },
            _ => Self::default(),
        }
    }
    pub fn configured_method(&self) -> Option<&Method> {
        self.method.as_ref()
    }
    /// A sufficient complete-domain proof, never a captured parameter sample.
    pub fn is_subset_of_prefix(&self, prefix: &str) -> bool {
        let Some(template) = self.template.as_ref().filter(|_| self.method.is_some()) else {
            return false;
        };
        let boundary = template
            .text
            .find(['{', '*'])
            .unwrap_or(template.text.len());
        template.text[..boundary].starts_with(prefix)
    }
    /// Does any admitted path start with this prefix? Unknown selectors return
    /// true so they cannot exclude an unsafe or anonymous branch.
    pub fn may_match_prefix(&self, prefix: &str) -> bool {
        let Some(template) = self.template.as_ref().filter(|_| self.method.is_some()) else {
            return true;
        };
        let Some(prefix) = prefix.strip_prefix('/') else {
            return true;
        };
        let mut parts = prefix.split('/').peekable();
        for segment in &template.segments {
            let Some(part) = parts.next() else {
                return true;
            };
            let last = parts.peek().is_none();
            match segment {
                Segment::Wildcard => return true,
                Segment::Literal(literal) if last => return literal.starts_with(part),
                Segment::Literal(literal) if literal == part => {}
                Segment::Parameter(_) if last => return true,
                Segment::Parameter(_) if !part.is_empty() => {}
                _ => return false,
            }
        }
        parts.next().is_none()
    }
}
impl Template {
    fn parse(path: &str) -> Result<Self, InitError> {
        if !path.starts_with('/') || path.contains(['?', '#', '@']) {
            return Err(InitError::Malformed("path"));
        }
        let parts: Vec<_> = path[1..].split('/').collect();
        let mut names = BTreeSet::new();
        let mut segments = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let segment = if *part == "*" && i + 1 == parts.len() {
                Segment::Wildcard
            } else if let Some(name) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                if name.is_empty()
                    || name.contains(['{', '}', '*'])
                    || !names.insert(name.to_owned())
                {
                    return Err(InitError::Malformed("parameter"));
                }
                Segment::Parameter(name.into())
            } else {
                if part.contains(['{', '}', '*']) {
                    return Err(InitError::Malformed("template"));
                }
                Segment::Literal((*part).into())
            };
            segments.push(segment);
        }
        Ok(Self {
            text: path.into(),
            segments,
        })
    }
    fn shape(&self) -> String {
        self.segments
            .iter()
            .map(|s| match s {
                Segment::Literal(s) => s.as_str(),
                Segment::Parameter(_) => "{}",
                Segment::Wildcard => "*",
            })
            .collect::<Vec<_>>()
            .join("/")
    }
    fn captures(&self, path: &str) -> Option<BTreeMap<String, String>> {
        let path = path.strip_prefix('/')?;
        let parts: Vec<_> = path.split('/').collect();
        let mut captures = BTreeMap::new();
        for (i, s) in self.segments.iter().enumerate() {
            match s {
                Segment::Wildcard => {
                    captures.insert(
                        "*".into(),
                        parts.get(i..).filter(|parts| !parts.is_empty())?.join("/"),
                    );
                    return Some(captures);
                }
                Segment::Literal(lit) if parts.get(i).copied() == Some(lit.as_str()) => {}
                Segment::Parameter(name) if parts.get(i).is_some_and(|p| !p.is_empty()) => {
                    captures.insert(name.clone(), parts[i].into());
                }
                _ => return None,
            }
        }
        (parts.len() == self.segments.len()).then_some(captures)
    }
    fn precedence(&self) -> Vec<u8> {
        // Lexicographic segment priority, then longest. No matcher-library defaults.
        // R-01a: deeper template outranks an overlapping wildcard.
        self.segments
            .iter()
            .map(|s| match s {
                Segment::Literal(_) => 3,
                Segment::Parameter(_) => 2,
                Segment::Wildcard => 1,
            })
            .collect()
    }
}
fn method(s: &str) -> Result<Method, InitError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-') {
        return Err(InitError::Malformed("method"));
    }
    Method::from_bytes(s.to_ascii_uppercase().as_bytes())
        .map_err(|_| InitError::Malformed("method"))
}

#[derive(Clone)]
struct Route<T> {
    template: Template,
    method: Method,
    value: T,
}
fn insert_route<T>(
    routes: &mut Vec<Route<T>>,
    path: &str,
    verb: &str,
    value: T,
) -> Result<(), InitError> {
    let template = Template::parse(path)?;
    let method = method(verb)?;
    if routes
        .iter()
        .any(|r| r.method == method && r.template.shape() == template.shape())
    {
        return Err(InitError::DuplicateRoute(format!("{path}@{method}")));
    }
    routes.push(Route {
        template,
        method,
        value,
    });
    routes.sort_by_key(|r| std::cmp::Reverse(r.template.precedence()));
    Ok(())
}
fn select<'a, T>(
    routes: &'a [Route<T>],
    inv: &Invocation,
) -> Option<(&'a Route<T>, BTreeMap<String, String>)> {
    routes
        .iter()
        .filter(|r| r.method.as_str().eq_ignore_ascii_case(inv.method.as_str()))
        .find_map(|r| r.template.captures(&inv.path).map(|c| (r, c)))
}
fn apply_match(
    inv: &mut Invocation,
    template: &str,
    params: BTreeMap<String, String>,
    target: Option<Target>,
) {
    for (key, value) in params {
        // R-03: captures override only the forwarded scalar query map. Keep the
        // normalized multimap intact for policies and the business v1 adapter.
        inv.path_params.insert(key, value);
    }
    inv.route = Some(RouteMatch {
        template: template.into(),
        target,
    });
}
/// Provider-neutral configured target mappings. Does not decode ingress paths,
/// expand ARNs, authorize AWS targets, or infer a destination from request input.
#[derive(Clone, Default)]
pub struct TargetRoutes {
    routes: Vec<Route<Target>>,
}
impl TargetRoutes {
    /// Observation-only matching: borrow immutable templates without allocating
    /// captures, changing invocation state, or selecting a backend destination.
    pub fn matching_template(&self, method: &Method, path: &str) -> Option<&str> {
        self.routes
            .iter()
            .filter(|route| route.method.as_str().eq_ignore_ascii_case(method.as_str()))
            .find(|route| matches_template_without_captures(&route.template, path))
            .map(|route| route.template.text.as_str())
    }
    pub fn compile(
        mappings: impl IntoIterator<Item = (String, Target)>,
    ) -> Result<Self, InitError> {
        let mut routes = Vec::new();
        for (mapping, target) in mappings {
            let Some((path, verb)) = mapping.split_once('@') else {
                return Err(InitError::Malformed("target mapping"));
            };
            let alias = match &target {
                Target::Function(s) | Target::HttpService(s) => s,
            };
            if alias.is_empty() {
                return Err(InitError::Malformed("target alias"));
            }
            insert_route(&mut routes, path, verb, target)?;
        }
        Ok(Self { routes })
    }
    pub fn resolve(&self, inv: &mut Invocation) -> Option<Target> {
        let (r, params) = select(&self.routes, inv)?;
        let target = r.value.clone();
        apply_match(inv, &r.template.text, params, Some(target.clone()));
        Some(target)
    }
}

fn matches_template_without_captures(template: &Template, path: &str) -> bool {
    let Some(path) = path.strip_prefix('/') else {
        return false;
    };
    let mut parts = path.split('/');
    for segment in &template.segments {
        let Some(part) = parts.next() else {
            return false;
        };
        match segment {
            Segment::Wildcard => return true,
            Segment::Literal(literal) if part == literal => {}
            Segment::Parameter(_) if !part.is_empty() => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

#[derive(Clone)]
pub(crate) enum Terminal {
    Dispatch(TargetRoutes),
    Local(Arc<dyn RequestHandler>),
}
pub(crate) struct CompiledChain {
    pub request: Vec<(Arc<dyn RequestHandler>, RequestCapabilities)>,
    pub terminal: Terminal,
    pub response: Vec<Arc<dyn ResponseHandler>>,
    ids: Vec<HandlerId>,
    pub media_roles: Vec<ValidatorMediaRole>,
}
pub struct ExecutionPlan {
    paths: Vec<Route<CompiledChain>>,
    default: Option<CompiledChain>,
    audit: AuditConfig,
    base_path: Option<String>,
}
impl ExecutionPlan {
    pub fn requires_validator_media(&self) -> bool {
        self.default
            .iter()
            .chain(self.paths.iter().map(|r| &r.value))
            .any(|chain| chain.media_roles.contains(&ValidatorMediaRole::Requires))
    }
    /// Read-only expanded selectable paths for per-Init Transformer aggregate
    /// admission. No caller can mutate plan or invocation destination state.
    pub fn transformer_occurrences(&self) -> Vec<Vec<HandlerId>> {
        self.default
            .iter()
            .chain(self.paths.iter().map(|route| &route.value))
            .map(|chain| {
                chain
                    .ids
                    .iter()
                    .copied()
                    .filter(|id| {
                        matches!(
                            id,
                            HandlerId::RequestTransformer | HandlerId::ResponseTransformer
                        )
                    })
                    .collect()
            })
            .collect()
    }
    /// Non-secret compiled metadata for the offline deployment validator.
    pub fn description(&self) -> (Vec<String>, Vec<String>) {
        let mut ids = BTreeSet::new();
        if let Some(chain) = &self.default {
            ids.extend(chain.ids.iter().copied());
        }
        for route in &self.paths {
            ids.extend(route.value.ids.iter().copied());
        }
        let handlers = ids.into_iter().map(|id| format!("{id:?}")).collect();
        let routes = self
            .paths
            .iter()
            .map(|route| format!("{}@{}", route.template.text, route.method))
            .collect();
        (handlers, routes)
    }
    /// Accepts effective JSON/YAML values after substitution. Audit comes from
    /// audit.yml separately; handler.audit* aliases are rejected.
    pub fn compile(handler: Value, audit: Value, registry: &Registry) -> Result<Self, InitError> {
        let h = handler
            .as_object()
            .ok_or(InitError::Malformed("handler object"))?;
        const KEYS: &[&str] = &[
            "enabled",
            "handlers",
            "additionalHandlers",
            "chains",
            "additionalChains",
            "paths",
            "additionalPaths",
            "defaultHandlers",
            "basePath",
        ];
        if h.keys().any(|k| !KEYS.contains(&k.as_str())) {
            return Err(InitError::Malformed("handler key"));
        }
        if h.get("enabled").is_some_and(|v| v != &Value::Bool(true)) {
            return Err(InitError::Malformed("handler must be enabled"));
        }
        let audit = if audit.is_null() {
            AuditConfig::default()
        } else {
            let values = audit
                .as_object()
                .ok_or(InitError::Malformed("audit configuration"))?;
            if values.keys().any(|key| key.starts_with("handler.")) {
                return Err(InitError::Malformed("audit ownership"));
            }
            let flag = |key| match values.get(key) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(value)) => Ok(*value),
                _ => Err(InitError::Malformed("audit flag")),
            };
            // Other audit.yml policy fields belong to its explicit WP12 factory.
            AuditConfig {
                audit_on_error: flag("auditOnError")?,
                audit_stack_trace: flag("auditStackTrace")?,
            }
        };
        let base_path = match h.get("basePath") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err(InitError::Malformed("basePath")),
        };
        // Java Handler stores basePath but does not prepend/strip it in getChain.
        // Keep that behavior. OpenAPI base-path interpretation belongs to its handler.
        let mut aliases = BTreeMap::new();
        for key in ["handlers", "additionalHandlers"] {
            for entry in strings(h.get(key))? {
                let (class, alias) = match entry.split_once('@') {
                    Some((c, a)) if !a.is_empty() && !a.contains('@') => (c, a),
                    None => (entry.as_str(), entry.as_str()),
                    _ => return Err(InitError::Malformed("handler mapping")),
                };
                let c =
                    compat::lookup(class).ok_or_else(|| InitError::UnknownClass(class.into()))?;
                // Java's later aliases replace earlier registrations.
                aliases.insert(alias.to_owned(), (c, registry.build(c, alias)?));
            }
        }
        let mut chains = BTreeMap::new();
        for key in ["chains", "additionalChains"] {
            let value = structured(h.get(key))?;
            if value.is_null() {
                continue;
            }
            for (name, exec) in value.as_object().ok_or(InitError::Malformed("chains"))? {
                if name.is_empty() {
                    return Err(InitError::Malformed("chain name"));
                }
                chains.insert(name.clone(), strings(Some(exec))?);
            }
        }
        // Validate even unused chains. Chains take precedence over same-name aliases.
        for name in chains.keys() {
            flatten(
                std::slice::from_ref(name),
                &chains,
                &aliases,
                &mut BTreeSet::new(),
            )?;
        }
        let default = if h
            .get("defaultHandlers")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
        {
            Some(compile_selectable_chain(flatten(
                &strings(h.get("defaultHandlers"))?,
                &chains,
                &aliases,
                &mut BTreeSet::new(),
            )?)?)
        } else {
            None
        };
        let mut paths = Vec::new();
        for key in ["paths", "additionalPaths"] {
            let value = structured(h.get(key))?;
            if value.is_null() {
                continue;
            }
            for entry in value.as_array().ok_or(InitError::Malformed("paths"))? {
                let p = entry
                    .as_object()
                    .ok_or(InitError::Malformed("path mapping"))?;
                if p.keys()
                    .any(|k| !["path", "method", "exec", "source"].contains(&k.as_str()))
                    || p.get("source").is_some_and(|v| !v.is_null())
                {
                    return Err(InitError::Malformed("path source or key"));
                }
                let path = p
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or(InitError::Malformed("path"))?;
                let verb = p
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or(InitError::Malformed("method"))?;
                let selection = OperationSelection::from_configured_route(path, verb);
                let chain = compile_selectable_chain_with_selection(
                    flatten(
                        &strings(p.get("exec"))?,
                        &chains,
                        &aliases,
                        &mut BTreeSet::new(),
                    )?,
                    Some(&selection),
                )?;
                insert_route(&mut paths, path, verb, chain)?;
            }
        }
        if paths.is_empty() && default.is_none() {
            return Err(InitError::MissingTerminal);
        }
        validate_metrics_templates(&paths, default.as_ref())?;
        Ok(Self {
            paths,
            default,
            audit,
            base_path,
        })
    }
    pub fn audit(&self) -> AuditConfig {
        self.audit
    }
    pub fn base_path(&self) -> Option<&str> {
        self.base_path.as_deref()
    }
    pub fn handler_ids(&self, inv: &Invocation) -> Option<&[HandlerId]> {
        select(&self.paths, inv)
            .map(|(r, _)| &r.value)
            .or(self.default.as_ref())
            .map(|c| c.ids.as_slice())
    }
    pub(crate) fn select(&self, inv: &mut Invocation) -> Option<&CompiledChain> {
        if let Some((r, params)) = select(&self.paths, inv) {
            apply_match(inv, &r.template.text, params, None);
            Some(&r.value)
        } else {
            self.default.as_ref()
        }
    }
}
fn structured(value: Option<&Value>) -> Result<Value, InitError> {
    match value {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Value::Null),
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|_| InitError::Malformed("structured value"))
        }
        Some(v) => Ok(v.clone()),
    }
}

const METRICS_TEMPLATE_BYTES: usize = 1024;

// Admission is scoped to selectable Metrics chains, not factory catalogues.
// Reachability uses the same literal/parameter/wildcard languages and ordering
// as selection, without captures, destinations, or request/policy execution.
fn validate_metrics_templates(
    paths: &[Route<CompiledChain>],
    default: Option<&CompiledChain>,
) -> Result<(), InitError> {
    for (index, path) in paths.iter().enumerate() {
        let blocked: Vec<_> = paths[..index]
            .iter()
            .filter(|other| other.method == path.method)
            .map(|other| &other.template)
            .collect();
        if path.template.text.len() > METRICS_TEMPLATE_BYTES
            && path.value.ids.contains(&HandlerId::Metrics)
            && template_language_has_witness(&[&path.template], &blocked)
        {
            return Err(metrics_template_error());
        }
        validate_chain_metrics_templates(&path.value, paths, Some(path))?;
    }
    if let Some(chain) = default {
        validate_chain_metrics_templates(chain, paths, None)?;
    }
    Ok(())
}

fn metrics_template_error() -> InitError {
    InitError::Malformed("metrics trusted route template byte limit (1024)")
}

fn validate_chain_metrics_templates(
    chain: &CompiledChain,
    paths: &[Route<CompiledChain>],
    selected: Option<&Route<CompiledChain>>,
) -> Result<(), InitError> {
    let metrics: Vec<_> = chain
        .request
        .iter()
        .filter(|(handler, _)| handler.id() == HandlerId::Metrics)
        .map(|(handler, _)| handler)
        .collect();
    if metrics.is_empty() {
        return Ok(());
    }
    let dispatch = match &chain.terminal {
        Terminal::Dispatch(routes) => Some(routes),
        Terminal::Local(_) => None,
    };
    for routes in dispatch.into_iter().chain(
        metrics
            .iter()
            .filter_map(|handler| handler.metrics_fallback_routes()),
    ) {
        for (index, route) in routes.routes.iter().enumerate() {
            if route.template.text.len() <= METRICS_TEMPLATE_BYTES
                || selected.is_some_and(|path| path.method != route.method)
            {
                continue;
            }
            let mut required = vec![&route.template];
            let mut blocked: Vec<_> = routes.routes[..index]
                .iter()
                .filter(|other| other.method == route.method)
                .map(|other| &other.template)
                .collect();
            if let Some(path) = selected {
                required.push(&path.template);
                blocked.extend(
                    paths
                        .iter()
                        .take_while(|other| !std::ptr::eq(*other, path))
                        .filter(|other| other.method == route.method)
                        .map(|other| &other.template),
                );
            } else {
                // The default is selectable only outside all configured paths.
                blocked.extend(
                    paths
                        .iter()
                        .filter(|other| other.method == route.method)
                        .map(|other| &other.template),
                );
            }
            if template_language_has_witness(&required, &blocked) {
                return Err(metrics_template_error());
            }
        }
    }
    Ok(())
}

// A finite symbolic alphabet: empty, each configured literal at this depth,
// and a nonempty value unequal to every literal. Beyond the longest template,
// only wildcard states remain, so one extra segment is sufficient. An explicit
// worklist avoids recursion even for large unrelated configuration templates.
fn template_language_has_witness(required: &[&Template], blocked: &[&Template]) -> bool {
    let limit = required
        .iter()
        .chain(blocked)
        .map(|template| template.segments.len())
        .max()
        .unwrap_or(0)
        + 1;
    let mut work = vec![(0, (0..blocked.len()).collect::<Vec<_>>())];
    let mut visited = BTreeSet::new();
    while let Some((depth, remaining)) = work.pop() {
        if !visited.insert((depth, remaining.clone())) {
            continue;
        }
        if depth > 0
            && required
                .iter()
                .all(|template| template_accepts(template, depth))
            && !remaining
                .iter()
                .any(|index| template_accepts(blocked[*index], depth))
        {
            return true;
        }
        if depth == limit {
            continue;
        }
        let mut literals = BTreeSet::from([""]);
        for template in required
            .iter()
            .copied()
            .chain(remaining.iter().map(|index| blocked[*index]))
        {
            if let Some(Segment::Literal(literal)) = template.segments.get(depth) {
                literals.insert(literal.as_str());
            }
        }
        for symbol in std::iter::once(None).chain(literals.into_iter().map(Some)) {
            if required
                .iter()
                .all(|template| template_consumes(template, depth, symbol))
            {
                work.push((
                    depth + 1,
                    remaining
                        .iter()
                        .copied()
                        .filter(|index| template_consumes(blocked[*index], depth, symbol))
                        .collect(),
                ));
            }
        }
    }
    false
}

fn template_accepts(template: &Template, depth: usize) -> bool {
    depth == template.segments.len()
        || (depth > template.segments.len()
            && matches!(template.segments.last(), Some(Segment::Wildcard)))
}

fn template_consumes(template: &Template, depth: usize, symbol: Option<&str>) -> bool {
    match template.segments.get(depth) {
        Some(Segment::Literal(literal)) => symbol == Some(literal.as_str()),
        Some(Segment::Parameter(_)) => symbol != Some(""),
        Some(Segment::Wildcard) => true,
        None => matches!(template.segments.last(), Some(Segment::Wildcard)),
    }
}
fn strings(value: Option<&Value>) -> Result<Vec<String>, InitError> {
    if let Some(Value::String(s)) = value
        && !s.trim().starts_with('[')
    {
        return Ok(if s.trim().is_empty() {
            vec![]
        } else {
            s.split(',').map(|v| v.trim().into()).collect()
        });
    }
    let v = structured(value)?;
    if v.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(v).map_err(|_| InitError::Malformed("string list"))
}
type Aliases = BTreeMap<
    String,
    (
        &'static Compatibility,
        Option<(Binding, ValidatorMediaRole)>,
    ),
>;
type SelectedBinding = (HandlerId, Binding, ValidatorMediaRole);
fn flatten(
    exec: &[String],
    chains: &BTreeMap<String, Vec<String>>,
    aliases: &Aliases,
    stack: &mut BTreeSet<String>,
) -> Result<Vec<SelectedBinding>, InitError> {
    let mut result = Vec::new();
    for name in exec {
        if let Some(chain) = chains.get(name) {
            if !stack.insert(name.clone()) {
                return Err(InitError::Cycle(name.clone()));
            }
            result.extend(flatten(chain, chains, aliases, stack)?);
            stack.remove(name);
        } else {
            let (c, binding) = aliases
                .get(name)
                .ok_or_else(|| InitError::UnknownAlias(name.clone()))?;
            if let Some((binding, media)) = binding {
                result.push((c.id, binding.clone(), *media));
            }
        }
    }
    Ok(result)
}
// Rust safety limit: count active request-policy occurrences after expansion,
// never catalogue entries or unused named chains. Repetitions remain distinct.
fn compile_selectable_chain(bindings: Vec<SelectedBinding>) -> Result<CompiledChain, InitError> {
    compile_selectable_chain_with_selection(bindings, None)
}
fn compile_selectable_chain_with_selection(
    bindings: Vec<SelectedBinding>,
    selection: Option<&OperationSelection>,
) -> Result<CompiledChain, InitError> {
    let completion_count = bindings
        .iter()
        .filter(|(id, binding, _)| {
            matches!(id, HandlerId::Audit | HandlerId::Metrics)
                && matches!(binding, Binding::Request(..))
        })
        .count();
    let chain = match selection {
        Some(selection) => compile_chain_with_selection(bindings, selection)?,
        None => compile_chain(bindings)?,
    };
    if completion_count > 64 {
        return Err(InitError::Malformed(
            "active audit/metrics occurrence limit (64)",
        ));
    }
    Ok(chain)
}
fn compile_chain(bindings: Vec<SelectedBinding>) -> Result<CompiledChain, InitError> {
    compile_chain_with_selection(bindings, &OperationSelection::default())
}
fn compile_chain_with_selection(
    bindings: Vec<SelectedBinding>,
    selection: &OperationSelection,
) -> Result<CompiledChain, InitError> {
    let unknown = OperationSelection::default();
    let mut current_selection = selection;
    let roles: Vec<_> = bindings
        .iter()
        .map(|(id, binding, _)| {
            let role = match binding {
                Binding::Request(handler, _) => {
                    handler.operation_authorization_role_for_selection(current_selection)
                }
                _ => OperationAuthorizationRole::None,
            };
            // These real policies have qualified immutable path/method behavior.
            // Unknown/new policies (including transformers) must not carry a proof
            // based on the incoming selector into a later prefix-auth decision.
            if matches!(binding, Binding::Request(..))
                && !matches!(
                    id,
                    HandlerId::Correlation
                        | HandlerId::Traceability
                        | HandlerId::RequestHeader
                        | HandlerId::RequestCors
                        | HandlerId::Jwt
                        | HandlerId::Security
                        | HandlerId::Basic
                        | HandlerId::ApiKey
                        | HandlerId::Limit
                        | HandlerId::Token
                        | HandlerId::Audit
                        | HandlerId::Metrics
                        | HandlerId::Specification
                )
            {
                current_selection = &unknown;
            }
            role
        })
        .collect();
    // Metadata-only observation chains and explicitly disabled scope policy keep
    // their existing behavior. A selected possible JWT scope path must have a
    // guaranteed check after the last producer; Basic/API-key paths never count.
    let has_scope_consumer = roles
        .iter()
        .any(|role| matches!(role, OperationAuthorizationRole::JwtScopes { .. }));
    let mut pending_operation_check = false;
    let mut has_operation_metadata = false;
    let mut request = Vec::new();
    let mut media_roles = Vec::new();
    let mut response = Vec::new();
    let mut terminal = None;
    let mut identity = false;
    let mut ids = Vec::new();
    for ((id, binding, media), role) in bindings.into_iter().zip(roles) {
        ids.push(id);
        match binding {
            Binding::Request(h, caps) => {
                if terminal.is_some() || !response.is_empty() {
                    return Err(InitError::InvalidTerminal);
                }
                if caps.requires_verified_identity && !identity {
                    return Err(InitError::IdentityOrdering);
                }
                identity |= caps.establishes_verified_identity;
                match role {
                    OperationAuthorizationRole::MetadataProducer => {
                        if id != HandlerId::Specification {
                            return Err(InitError::FactoryContract(id));
                        }
                        pending_operation_check = has_scope_consumer;
                        has_operation_metadata = true;
                    }
                    OperationAuthorizationRole::MetadataConsumer => {
                        if id != HandlerId::Validator {
                            return Err(InitError::FactoryContract(id));
                        }
                        if !has_operation_metadata {
                            return Err(InitError::Malformed(
                                "validator requires preceding qualified Specification metadata",
                            ));
                        }
                    }
                    OperationAuthorizationRole::JwtScopes { guaranteed } => {
                        if !matches!(id, HandlerId::Jwt | HandlerId::Security) {
                            return Err(InitError::FactoryContract(id));
                        }
                        if guaranteed {
                            pending_operation_check = false;
                        }
                    }
                    OperationAuthorizationRole::None => {}
                }
                request.push((h, caps));
                media_roles.push(media);
            }
            Binding::Response(h) => {
                if terminal.is_none() {
                    return Err(InitError::InvalidTerminal);
                }
                response.push(h);
            }
            Binding::Dispatch(routes) => {
                if terminal.replace(Terminal::Dispatch(routes)).is_some() {
                    return Err(InitError::InvalidTerminal);
                }
            }
            Binding::Local(h) => {
                if terminal.replace(Terminal::Local(h)).is_some() {
                    return Err(InitError::InvalidTerminal);
                }
            }
        }
    }
    let terminal = terminal.ok_or(InitError::MissingTerminal)?;
    if pending_operation_check {
        return Err(InitError::Malformed(
            "operation metadata requires a later guaranteed JWT scope check",
        ));
    }
    let mut known_media = true;
    for ((handler, _), role) in request.iter().zip(&media_roles) {
        match role {
            ValidatorMediaRole::Unspecified => known_media = false,
            ValidatorMediaRole::Requires => {
                if handler.id() != HandlerId::Validator {
                    return Err(InitError::FactoryContract(handler.id()));
                }
                if !known_media {
                    return Err(InitError::Malformed(
                        "validator requires qualified current header provenance",
                    ));
                }
            }
            ValidatorMediaRole::Preserves | ValidatorMediaRole::Maintains => {}
        }
    }
    Ok(CompiledChain {
        media_roles,
        request,
        response,
        terminal,
        ids,
    })
}
