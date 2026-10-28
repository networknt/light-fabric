//! Opt-in rule preparation. Legacy loaders and their public behavior are unchanged.
use super::*;
use std::{collections::BTreeMap, ffi::CStr, io::Read, mem::MaybeUninit};
use unsafe_libyaml as unsafe_yaml;
pub const REJECTED: &str = "transformer: unsupported rule profile";
const MALFORMED_YAML: &str = "bounded YAML syntax failure";
static INTERPOLATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([^}]+)\}").expect("fixed variable regex"));
fn invalid() -> ConfigError {
    ConfigError::Convert(REJECTED.into())
}
pub struct RuleBudget;
impl RuleBudget {
    pub const RAW: usize = 1048576;
    pub const CAPTURES: usize = 2097152;
    pub const EVENTS: usize = 65536;
    pub const SYNTAX_DEPTH: usize = 64;
    pub const NODES: usize = 16384;
    pub const DEPTH: usize = 16;
    pub const BYTES: usize = 65536;
}
/// Per-preparation capture only; no mutable cross-Init memo or request access.
#[derive(Default)]
pub struct BoundedCaptures {
    used: usize,
    files: BTreeMap<PathBuf, Arc<[u8]>>,
}
impl BoundedCaptures {
    pub fn bytes(&self) -> usize {
        self.used
    }
    pub fn read(&mut self, path: &Path) -> Result<Arc<[u8]>, ConfigError> {
        if let Some(bytes) = self.files.get(path) {
            return Ok(bytes.clone());
        }
        let remaining = RuleBudget::CAPTURES
            .checked_sub(self.used)
            .ok_or_else(invalid)?
            .min(RuleBudget::RAW);
        let mut file = fs::File::open(path)?;
        // Bound before reserve/read; one sentinel byte is explicit overhead.
        let mut bytes = Vec::with_capacity(remaining + 1);
        let mut scratch = [0u8; 8192];
        while bytes.len() < remaining + 1 {
            let count = scratch.len().min(remaining + 1 - bytes.len());
            let read = file.read(&mut scratch[..count])?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&scratch[..read]);
        }
        if bytes.len() > remaining {
            return Err(invalid());
        }
        self.used += bytes.len();
        let bytes: Arc<[u8]> = bytes.into();
        self.files.insert(path.into(), bytes.clone());
        Ok(bytes)
    }
    pub fn embedded(&mut self, file: &EmbeddedConfigFile) -> Result<Arc<[u8]>, ConfigError> {
        let key = PathBuf::from(format!("<embedded>/{}", file.name));
        if let Some(bytes) = self.files.get(&key) {
            return Ok(bytes.clone());
        }
        if file.content.len() > RuleBudget::RAW
            || file.content.len() > RuleBudget::CAPTURES - self.used
        {
            return Err(invalid());
        }
        self.used += file.content.len();
        let bytes: Arc<[u8]> = file.content.as_bytes().into();
        self.files.insert(key, bytes.clone());
        Ok(bytes)
    }
}

/// An opt-in, Init-local source selection. Publication exposes immutable bytes
/// only; later stages never inspect the filesystem to reconstruct this snapshot.
pub struct BoundedLocalSources {
    values: Vec<(Option<PathBuf>, Arc<[u8]>, bool)>,
    rule: Result<Option<Arc<[u8]>>, ConfigError>,
    bytes: usize,
}
impl BoundedLocalSources {
    pub fn capture(
        embedded: &[EmbeddedConfigFile],
        default_dir: Option<&Path>,
        local: &Path,
        external: &Path,
    ) -> Result<Self, ConfigError> {
        let mut captures = BoundedCaptures::default();
        let mut values = Vec::new();
        if let Some(file) = embedded_config_file(embedded, "values.yml") {
            values.push((None, captures.embedded(file)?, true));
        }
        let bootstrap = existing_config_paths(default_dir, local, None, "values.yml");
        for path in existing_config_paths(default_dir, local, Some(external), "values.yml") {
            let bytes = captures.read(&path)?;
            values.push((Some(path.clone()), bytes, bootstrap.contains(&path)));
        }
        let rule = if let Some(path) =
            existing_config_paths(default_dir, local, Some(external), "rule.yml").last()
        {
            captures.read(path).map(Some)
        } else if let Some(file) = embedded_config_file(embedded, "rule.yml") {
            captures.embedded(file).map(Some)
        } else {
            Ok(None)
        };
        // Complete capture before parsing any participating values source.
        Ok(Self {
            values,
            rule,
            bytes: captures.bytes(),
        })
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn source(&self, path: &Path) -> Option<&[u8]> {
        self.values
            .iter()
            .find(|(p, _, _)| p.as_deref() == Some(path))
            .map(|(_, bytes, _)| bytes.as_ref())
    }
    pub fn values(&self, bootstrap_only: bool) -> Result<HashMap<String, Value>, ConfigError> {
        // Check every participating document before the first Value allocation,
        // including documents unused by bootstrap or overridden by precedence.
        let mut total = Cost::default();
        for (_, bytes, _) in &self.values {
            let cost = expanded_preflight(
                bytes,
                RuleBudget::RAW,
                RuleBudget::EVENTS,
                RuleBudget::SYNTAX_DEPTH,
            )?;
            total.bytes = total.bytes.checked_add(cost.bytes).ok_or_else(invalid)?;
            total.nodes = total.nodes.checked_add(cost.nodes).ok_or_else(invalid)?;
            if total.bytes > RuleBudget::CAPTURES || total.nodes > RuleBudget::EVENTS {
                return Err(invalid());
            }
        }
        let mut result = HashMap::new();
        for (_, bytes, bootstrap) in &self.values {
            if bootstrap_only && !bootstrap {
                continue;
            }
            let content = std::str::from_utf8(bytes).map_err(|_| invalid())?;
            result.extend(parse_values_content(content).map_err(|_| invalid())?);
        }
        Ok(result)
    }
    pub fn rule(&self) -> Result<Option<Value>, ConfigError> {
        self.rule
            .as_ref()
            .map_err(|_| invalid())?
            .as_deref()
            .map(parse)
            .transpose()
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct Cost {
    bytes: usize,
    nodes: usize,
    depth: usize,
}
impl Cost {
    fn check(self) -> Result<Self, ConfigError> {
        if self.bytes > RuleBudget::BYTES
            || self.nodes > RuleBudget::NODES
            || self.depth > RuleBudget::DEPTH
        {
            Err(invalid())
        } else {
            Ok(self)
        }
    }
}
fn quoted(s: &str) -> usize {
    2 + s
        .chars()
        .map(|c| match c {
            '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
            c if (c as u32) < 32 => 6,
            c => c.len_utf8(),
        })
        .sum::<usize>()
}
fn measure(value: &Value) -> Result<Cost, ConfigError> {
    fn visit(value: &Value, depth: usize, cost: &mut Cost) -> Result<(), ConfigError> {
        cost.nodes += 1;
        cost.depth = cost.depth.max(depth);
        cost.bytes += match value {
            Value::Null => 4,
            Value::Bool(true) => 4,
            Value::Bool(false) => 5,
            Value::Number(n) => n.to_string().len(),
            Value::String(s) => {
                if s.len() > RuleBudget::BYTES {
                    return Err(invalid());
                }
                quoted(s)
            }
            Value::Sequence(v) => 2 + v.len().saturating_sub(1),
            Value::Mapping(v) => 2 + v.len().saturating_sub(1),
            Value::Tagged(v) => {
                cost.nodes -= 1;
                return visit(&v.value, depth, cost);
            }
        };
        cost.check()?;
        match value {
            Value::Sequence(v) => {
                for v in v {
                    visit(v, depth + 1, cost)?;
                }
            }
            Value::Mapping(v) => {
                for (k, v) in v {
                    let key = k.as_str().ok_or_else(invalid)?;
                    if key.len() > RuleBudget::BYTES {
                        return Err(invalid());
                    }
                    cost.bytes += quoted(key) + 1;
                    cost.check()?;
                    visit(v, depth + 1, cost)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut cost = Cost::default();
    visit(value, 0, &mut cost)?;
    Ok(cost)
}
struct Parser(Box<unsafe_yaml::yaml_parser_t>);
impl Drop for Parser {
    fn drop(&mut self) {
        unsafe { unsafe_yaml::yaml_parser_delete(&mut *self.0) }
    }
}
struct Event(unsafe_yaml::yaml_event_t);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe { unsafe_yaml::yaml_event_delete(&mut self.0) }
    }
}
fn text(event: &Event, ptr: *const u8) -> Result<&str, ConfigError> {
    let _ = event; // Tie borrowed event data to its deletion guard.
    if ptr.is_null() {
        return Ok("");
    }
    let bytes = unsafe { CStr::from_ptr(ptr.cast()).to_bytes() };
    if bytes.len() > RuleBudget::RAW {
        return Err(invalid());
    }
    std::str::from_utf8(bytes).map_err(|_| invalid())
}
struct Frame {
    cost: Cost,
    mapping: bool,
    key: bool,
    anchor: String,
    entries: usize,
}
fn scalar_bytes(input: &str) -> Result<usize, ConfigError> {
    struct Scalar<'a>(&'a str);
    impl<'de> serde::de::Visitor<'de> for Scalar<'_> {
        type Value = usize;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("scalar")
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<usize, E> {
            Ok(quoted(self.0))
        }
        fn visit_bool<E: serde::de::Error>(self, b: bool) -> Result<usize, E> {
            Ok(if b { 4 } else { 5 })
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<usize, E> {
            Ok(4)
        }
        fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<usize, E> {
            Ok(n.to_string().len())
        }
        fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<usize, E> {
            Ok(n.to_string().len())
        }
        fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<usize, E> {
            Ok(serde_yaml::Number::from(n).to_string().len())
        }
    }
    use serde::Deserializer;
    // This parser's input is already RAW-bounded. The visitor never creates a
    // Value/string copy, including for a decoded scalar larger than BYTES.
    serde_yaml::Deserializer::from_str(input)
        .deserialize_any(Scalar(input))
        .map_err(|_| invalid())
}
/// A streaming first pass checks event/scalar buffers and alias-expanded cost
/// before serde_yaml may materialize its whole event document or expanded Value.
fn preflight(raw: &[u8]) -> Result<(), ConfigError> {
    expanded_preflight(raw, RuleBudget::BYTES, RuleBudget::NODES, RuleBudget::DEPTH).map(|_| ())
}
fn expanded_preflight(
    raw: &[u8],
    byte_limit: usize,
    node_limit: usize,
    depth_limit: usize,
) -> Result<Cost, ConfigError> {
    let check = |cost: Cost| {
        if cost.bytes > byte_limit || cost.nodes > node_limit || cost.depth > depth_limit {
            Err(invalid())
        } else {
            Ok(cost)
        }
    };
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(Cost::default());
    }
    if raw.len() > RuleBudget::RAW {
        return Err(invalid());
    }
    let mut uninit = Box::new(MaybeUninit::<unsafe_yaml::yaml_parser_t>::uninit());
    unsafe {
        if unsafe_yaml::yaml_parser_initialize(uninit.as_mut_ptr()).fail {
            return Err(invalid());
        }
    }
    let mut parser = Parser(unsafe { uninit.assume_init() });
    unsafe {
        unsafe_yaml::yaml_parser_set_input_string(&mut *parser.0, raw.as_ptr(), raw.len() as u64);
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors = BTreeMap::<String, (Cost, Option<usize>)>::new();
    let mut root = None;
    let mut docs = 0;
    let mut buffered = 0usize;
    for _ in 0..RuleBudget::EVENTS {
        let mut event = MaybeUninit::<unsafe_yaml::yaml_event_t>::uninit();
        if unsafe { unsafe_yaml::yaml_parser_parse(&mut *parser.0, event.as_mut_ptr()) }.fail {
            return Err(ConfigError::Convert(MALFORMED_YAML.into()));
        }
        let event = Event(unsafe { event.assume_init() });
        let (cost, anchor, key_bytes) = unsafe {
            match event.0.type_ {
                unsafe_yaml::YAML_STREAM_START_EVENT | unsafe_yaml::YAML_DOCUMENT_END_EVENT => {
                    continue;
                }
                unsafe_yaml::YAML_DOCUMENT_START_EVENT => {
                    docs += 1;
                    if docs > 1 {
                        return Err(invalid());
                    }
                    continue;
                }
                unsafe_yaml::YAML_STREAM_END_EVENT => {
                    return match root {
                        Some(cost) if stack.is_empty() => Ok(cost),
                        None if docs == 0 && stack.is_empty() => Ok(Cost::default()),
                        _ => Err(invalid()),
                    };
                }
                unsafe_yaml::YAML_SEQUENCE_START_EVENT | unsafe_yaml::YAML_MAPPING_START_EVENT => {
                    if stack.len() >= RuleBudget::SYNTAX_DEPTH {
                        return Err(invalid());
                    }
                    let mapping = event.0.type_ == unsafe_yaml::YAML_MAPPING_START_EVENT;
                    let (anchor, tag) = if mapping {
                        (
                            event.0.data.mapping_start.anchor,
                            event.0.data.mapping_start.tag,
                        )
                    } else {
                        (
                            event.0.data.sequence_start.anchor,
                            event.0.data.sequence_start.tag,
                        )
                    };
                    let anchor = text(&event, anchor)?;
                    let tag = text(&event, tag)?;
                    buffered = buffered
                        .checked_add(anchor.len() + tag.len())
                        .ok_or_else(invalid)?;
                    if buffered > RuleBudget::RAW {
                        return Err(invalid());
                    }
                    stack.push(Frame {
                        cost: Cost {
                            bytes: 2,
                            nodes: 1,
                            depth: 0,
                        },
                        mapping,
                        key: mapping,
                        anchor: anchor.into(),
                        entries: 0,
                    });
                    continue;
                }
                unsafe_yaml::YAML_SEQUENCE_END_EVENT | unsafe_yaml::YAML_MAPPING_END_EVENT => {
                    let f = stack.pop().ok_or_else(invalid)?;
                    if f.mapping && !f.key {
                        return Err(invalid());
                    }
                    (check(f.cost)?, f.anchor, None)
                }
                unsafe_yaml::YAML_ALIAS_EVENT => {
                    let anchor = text(&event, event.0.data.alias.anchor)?;
                    {
                        let (mut cost, key_bytes) = *anchors.get(anchor).ok_or_else(invalid)?;
                        if stack.last().is_some_and(|f| f.mapping && f.key) {
                            cost.bytes = key_bytes.ok_or_else(invalid)?;
                        }
                        (check(cost)?, String::new(), key_bytes)
                    }
                }
                unsafe_yaml::YAML_SCALAR_EVENT => {
                    let scalar = event.0.data.scalar;
                    if scalar.length as usize > RuleBudget::RAW {
                        return Err(invalid());
                    }
                    let value = std::str::from_utf8(std::slice::from_raw_parts(
                        scalar.value,
                        scalar.length as usize,
                    ))
                    .map_err(|_| invalid())?;
                    let tag = text(&event, scalar.tag)?;
                    let anchor = text(&event, scalar.anchor)?;
                    buffered = buffered
                        .checked_add(value.len() + tag.len() + anchor.len())
                        .ok_or_else(invalid)?;
                    if buffered > RuleBudget::RAW {
                        return Err(invalid());
                    }
                    let is_key = stack.last().is_some_and(|f| f.mapping && f.key);
                    let bytes = if is_key
                        || tag == "tag:yaml.org,2002:str"
                        || (tag.is_empty() && scalar.style != unsafe_yaml::YAML_PLAIN_SCALAR_STYLE)
                    {
                        quoted(value)
                    } else {
                        // Temporary scalar parsing is bounded by RAW before this
                        // call, independently of expanded-document materialization.
                        scalar_bytes(value)?
                    };
                    (
                        check(Cost {
                            bytes,
                            nodes: 1,
                            depth: 0,
                        })?,
                        anchor.to_owned(),
                        Some(quoted(value)),
                    )
                }
                _ => return Err(invalid()),
            }
        };
        if !anchor.is_empty() {
            anchors.insert(anchor, (cost, key_bytes));
        }
        if let Some(frame) = stack.last_mut() {
            if frame.mapping && frame.key {
                if cost.nodes != 1 {
                    return Err(invalid());
                }
                frame.cost.bytes += cost.bytes + 1;
                frame.key = false;
            } else {
                frame.cost.bytes += cost.bytes + usize::from(frame.entries > 0);
                frame.cost.nodes += cost.nodes;
                frame.cost.depth = frame.cost.depth.max(cost.depth + 1);
                frame.entries += 1;
                if frame.mapping {
                    frame.key = true;
                }
            }
            check(frame.cost)?;
        } else if root.replace(cost).is_some() {
            return Err(invalid());
        }
    }
    Err(invalid())
}
fn parse(raw: &[u8]) -> Result<Value, ConfigError> {
    preflight(raw).map_err(|_| invalid())?;
    let value = serde_yaml::from_slice(raw).map_err(|_| invalid())?;
    measure(&value)?;
    Ok(value)
}
impl ConfigLoader {
    /// Parse an already captured document using the existing bounded profile.
    /// This never reopens a path or changes the legacy loading methods.
    pub fn parse_captured_config_bounded(raw: &[u8]) -> Result<Value, ConfigError> {
        parse(raw)
    }

    /// Check every values layer before materializing any of them, then merge in
    /// caller-supplied precedence order. Overridden layers are still checked.
    pub fn parse_captured_values_bounded(
        layers: &[Arc<[u8]>],
    ) -> Result<HashMap<String, Value>, ConfigError> {
        let mut total = Cost::default();
        for raw in layers {
            let cost = expanded_preflight(
                raw,
                RuleBudget::RAW,
                RuleBudget::EVENTS,
                RuleBudget::SYNTAX_DEPTH,
            )?;
            total.bytes = total.bytes.checked_add(cost.bytes).ok_or_else(invalid)?;
            total.nodes = total.nodes.checked_add(cost.nodes).ok_or_else(invalid)?;
            if total.bytes > RuleBudget::CAPTURES || total.nodes > RuleBudget::EVENTS {
                return Err(invalid());
            }
        }
        let mut values = HashMap::new();
        for raw in layers {
            let content = std::str::from_utf8(raw).map_err(|_| invalid())?;
            values.extend(parse_values_content(content).map_err(|_| invalid())?);
        }
        Ok(values)
    }

    pub fn load_file_bounded(
        &self,
        path: impl AsRef<Path>,
        captures: &mut BoundedCaptures,
    ) -> Result<Value, ConfigError> {
        parse(&captures.read(path.as_ref())?)
    }
    pub fn load_embedded_file_bounded(
        &self,
        file: &EmbeddedConfigFile,
        captures: &mut BoundedCaptures,
    ) -> Result<Value, ConfigError> {
        parse(&captures.embedded(file)?)
    }
    /// Source values are borrowed: unrelated application values are never cloned.
    /// The standard environment retrieval API still owns its returned source
    /// String; this method does not claim a bound on that retrieval allocation.
    pub fn resolve_value_bounded(
        &self,
        value: &mut Value,
        values: &HashMap<String, Value>,
    ) -> Result<(), ConfigError> {
        measure(value)?;
        let mut used = Cost::default();
        let next = self.resolve_bounded_pass(value, values, 0, &mut used, 0)?;
        *value = next;
        Ok(())
    }
    fn resolve_bounded_pass(
        &self,
        value: &Value,
        values: &HashMap<String, Value>,
        depth: usize,
        used: &mut Cost,
        references: usize,
    ) -> Result<Value, ConfigError> {
        if let Value::String(input) = value {
            if let Some(caps) = WHOLE_VARIABLE_REGEX.captures(input) {
                let key = caps.get(1).ok_or_else(invalid)?.as_str();
                // Compatibility precedence is environment, typed values, default.
                if let Some(source) = self.get_env_value(key) {
                    return self.copy_resolved_scalar(&source, true, depth, used);
                }
                if let Some(source) = values.get(key).or_else(|| self.values.get(key)) {
                    if references >= MAX_EXPANSION_DEPTH {
                        return Err(invalid());
                    }
                    let mut bound = measure(source)?;
                    bound.depth += depth;
                    let mut check = *used;
                    charge(bound, &mut check)?;
                    return self.resolve_bounded_pass(source, values, depth, used, references + 1);
                }
                if let Some(default) = caps.get(2) {
                    return self.copy_resolved_scalar(default.as_str(), true, depth, used);
                }
                return Err(invalid());
            }
            // Match legacy scalar interpolation and whole-variable semantics;
            // each generated intermediate is admitted before append.
            let re = &*INTERPOLATION;
            let mut owned = None::<String>;
            for _ in 0..MAX_EXPANSION_DEPTH {
                let current = owned.as_deref().unwrap_or(input);
                if !re.is_match(current) {
                    return self.copy_resolved_scalar(current, false, depth, used);
                }
                let mut output = String::new();
                let mut at = 0;
                for caps in re.captures_iter(current) {
                    let span = caps.get(0).ok_or_else(invalid)?;
                    append(&mut output, &current[at..span.start()], used)?;
                    let inner = caps.get(1).ok_or_else(invalid)?.as_str();
                    let (key, default) = inner
                        .split_once(':')
                        .map_or((inner, None), |(k, v)| (k, Some(v)));
                    if let Some(source) = self.get_env_value(key) {
                        append(&mut output, &source, used)?;
                    } else if let Some(source) = values.get(key).or_else(|| self.values.get(key)) {
                        match source {
                            Value::String(s) => append(&mut output, s, used)?,
                            Value::Number(n) => append(&mut output, &n.to_string(), used)?,
                            Value::Bool(b) => {
                                append(&mut output, if *b { "true" } else { "false" }, used)?
                            }
                            _ => append(&mut output, default.unwrap_or(span.as_str()), used)?,
                        }
                    } else {
                        append(&mut output, default.unwrap_or(span.as_str()), used)?;
                    }
                    at = span.end();
                }
                append(&mut output, &current[at..], used)?;
                if output == current {
                    return Err(invalid());
                }
                owned = Some(output);
            }
            if let Some(output) = owned.as_deref().filter(|s| !re.is_match(s)) {
                self.copy_resolved_scalar(output, false, depth, used)
            } else {
                Err(invalid())
            }
        } else {
            match value {
                Value::Sequence(seq) => {
                    charge(
                        Cost {
                            bytes: 2 + seq.len().saturating_sub(1),
                            nodes: 1,
                            depth,
                        },
                        used,
                    )?;
                    let mut output = Vec::with_capacity(seq.len());
                    for value in seq {
                        output.push(self.resolve_bounded_pass(
                            value,
                            values,
                            depth + 1,
                            used,
                            references,
                        )?);
                    }
                    Ok(Value::Sequence(output))
                }
                Value::Mapping(map) => {
                    charge(
                        Cost {
                            bytes: 2 + map.len().saturating_sub(1),
                            nodes: 1,
                            depth,
                        },
                        used,
                    )?;
                    let mut output = Mapping::new();
                    for (key, value) in map {
                        let text = key.as_str().ok_or_else(invalid)?;
                        charge(
                            Cost {
                                bytes: quoted(text) + 1,
                                nodes: 0,
                                depth,
                            },
                            used,
                        )?;
                        let value =
                            self.resolve_bounded_pass(value, values, depth + 1, used, references)?;
                        output.insert(Value::String(text.into()), value);
                    }
                    Ok(Value::Mapping(output))
                }
                _ => copy_checked(value, depth, used),
            }
        }
    }
    fn copy_resolved_scalar(
        &self,
        input: &str,
        typed: bool,
        depth: usize,
        used: &mut Cost,
    ) -> Result<Value, ConfigError> {
        if input.len() > RuleBudget::BYTES {
            return Err(invalid());
        }
        // Encoded encrypted payload is admitted before existing decryption is
        // called. AES hex and RSA base64 plaintext cannot exceed these bytes;
        // cryptographic implementation/key scratch is accounted separately.
        let source = Cost {
            bytes: quoted(input),
            nodes: 1,
            depth,
        };
        let mut check = *used;
        charge(source, &mut check)?;
        let decrypted = self.resolve_scalar_from_text(input.into(), false);
        let text = decrypted.as_str().ok_or_else(invalid)?;
        // Returned source/decryption buffers are outside the loader-copy bound;
        // inspect them before any additional parse, clone or expansion.
        if text.len() > RuleBudget::BYTES {
            return Err(invalid());
        }
        let value = if typed {
            if matches!(text.trim().as_bytes().first(), Some(b'[' | b'{')) {
                // Syntax failures retain the legacy string fallback. Budget
                // failures must never fall back or materialize an oversized tree.
                match preflight(text.as_bytes()) {
                    Err(ConfigError::Convert(message)) if message == MALFORMED_YAML => {
                        return copy_checked(&Value::String(text.to_owned()), depth, used);
                    }
                    Err(_) => return Err(invalid()),
                    Ok(()) => parse(text.as_bytes())?,
                }
            } else {
                Self::parse_text_scalar(text)
            }
        } else {
            decrypted
        };
        copy_checked(&value, depth, used)
    }
}
fn charge(cost: Cost, used: &mut Cost) -> Result<(), ConfigError> {
    used.bytes = used.bytes.checked_add(cost.bytes).ok_or_else(invalid)?;
    used.nodes = used.nodes.checked_add(cost.nodes).ok_or_else(invalid)?;
    used.depth = used.depth.max(cost.depth);
    used.check()?;
    Ok(())
}
fn copy_checked(value: &Value, depth: usize, used: &mut Cost) -> Result<Value, ConfigError> {
    let mut cost = measure(value)?;
    cost.depth += depth;
    charge(cost, used)?;
    Ok(value.clone())
}
fn append(output: &mut String, part: &str, used: &Cost) -> Result<(), ConfigError> {
    if part.len() > RuleBudget::BYTES.saturating_sub(used.bytes) {
        return Err(invalid());
    }
    let size = quoted(output)
        .checked_add(quoted(part) - 2)
        .ok_or_else(invalid)?;
    if size > RuleBudget::BYTES.saturating_sub(used.bytes) {
        return Err(invalid());
    }
    output.push_str(part);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn loader() -> ConfigLoader {
        ConfigLoader::from_values(Default::default(), None, None).unwrap()
    }
    #[test]
    fn aliases_events_depth_and_effective_exact_boundaries_before_materialization() {
        let raw = b"a: &a [one, two]\nb: *a\n";
        assert_eq!(
            parse(raw).unwrap(),
            serde_yaml::from_slice::<Value>(raw).unwrap()
        );
        let exact = format!("'{}'", "x".repeat(RuleBudget::BYTES - 2));
        assert!(parse(exact.as_bytes()).is_ok());
        let excess = format!("'{}'", "x".repeat(RuleBudget::BYTES - 1));
        assert!(parse(excess.as_bytes()).is_err());
        let mut expansion = String::from("a: &a [x,x,x,x,x,x,x,x]\n");
        for (next, previous) in [('b', 'a'), ('c', 'b'), ('d', 'c'), ('e', 'd')] {
            expansion.push_str(&format!(
                "{next}: &{next} [{0},{0},{0},{0},{0},{0},{0},{0}]\n",
                format!("*{previous}")
            ));
        }
        assert!(parse(expansion.as_bytes()).is_err());
        assert!(parse(b"a: &a [*a]").is_err());
        assert!(parse(format!("{}x{}", "[".repeat(65), "]".repeat(65)).as_bytes()).is_err());
        assert!(parse(b"a: 1\n---\nb: 2").is_err());
        let exact_nodes = format!("[{}]", vec!["0"; RuleBudget::NODES - 1].join(","));
        assert!(parse(exact_nodes.as_bytes()).is_ok());
        let extra_node = format!("[{}]", vec!["0"; RuleBudget::NODES].join(","));
        assert!(parse(extra_node.as_bytes()).is_err());
        assert!(parse(format!("{}0{}", "[".repeat(16), "]".repeat(16)).as_bytes()).is_ok());
        assert!(parse(format!("{}0{}", "[".repeat(17), "]".repeat(17)).as_bytes()).is_err());
    }
    #[test]
    fn substitution_is_typed_borrowed_aggregate_bounded_and_cycle_rejected() {
        let mut values = HashMap::new();
        values.insert(
            "typed".into(),
            serde_yaml::from_str("{a: [1, true]}").unwrap(),
        );
        values.insert("huge".into(), Value::String("x".repeat(RuleBudget::BYTES)));
        values.insert("cycle".into(), Value::String("${cycle}".into()));
        let mut value: Value =
            serde_yaml::from_str("a: ${typed}\nb: pre-${none:default}\nc: ${missing:false}")
                .unwrap();
        loader().resolve_value_bounded(&mut value, &values).unwrap();
        let compatible = ConfigLoader::from_values(values.clone(), None, None).unwrap();
        for text in [
            "a: ${typed}",
            "a: pre-${missing:default}",
            "a: ${missing:false}",
            "a: ${missing:${typed}}",
            "a: literal-$-brace-${",
        ] {
            let mut legacy: Value = serde_yaml::from_str(text).unwrap();
            let mut bounded = legacy.clone();
            let before = compatible.resolve_value(&mut legacy);
            let after = loader().resolve_value_bounded(&mut bounded, &values);
            assert_eq!(before.is_ok(), after.is_ok(), "{text}");
            if before.is_ok() {
                assert_eq!(legacy, bounded, "{text}");
            }
        }
        assert_eq!(value["a"]["a"][0], Value::Number(1.into()));
        assert_eq!(value["b"].as_str(), Some("pre-default"));
        assert_eq!(value["c"], Value::Bool(false));
        for text in ["a: ${huge}", "a: prefix-${huge}", "a: ${cycle}"] {
            let mut value = serde_yaml::from_str(text).unwrap();
            assert!(loader().resolve_value_bounded(&mut value, &values).is_err());
        }
        // Large unrelated source values are borrowed and not copied.
        let mut value = serde_yaml::from_str("a: small").unwrap();
        loader().resolve_value_bounded(&mut value, &values).unwrap();
    }
    #[test]
    fn immutable_raw_capture_limits_include_comments_and_combined_values() {
        let dir = std::env::temp_dir().join(format!("wp12-rule-capture-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let rule = dir.join("rule.yml");
        let values = dir.join("values.yml");
        let mut raw = vec![b' '; RuleBudget::RAW];
        raw[0] = b'#';
        fs::write(&rule, &raw).unwrap();
        fs::write(&values, &raw).unwrap();
        let mut captures = BoundedCaptures::default();
        let captured = captures.read(&rule).unwrap();
        fs::write(&rule, "changed after capture").unwrap();
        assert_eq!(&*captures.read(&rule).unwrap(), &*captured);
        captures.read(&values).unwrap();
        assert_eq!(captures.bytes(), RuleBudget::CAPTURES);
        let extra = dir.join("extra.yml");
        fs::write(&extra, "x").unwrap();
        assert!(captures.read(&extra).is_err());
        raw.push(b' ');
        fs::write(&values, &raw).unwrap();
        assert!(BoundedCaptures::default().read(&values).is_err());
        // Owned scratch only; no repository/evidence cleanup.
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    #[test]
    fn comment_only_yaml_is_an_empty_document() {
        for raw in [
            b"# comment\n".as_slice(),
            b"  # values\n# rule\n".as_slice(),
        ] {
            assert_eq!(
                ConfigLoader::parse_captured_config_bounded(raw).unwrap(),
                Value::Null
            );
        }
    }
    #[test]
    fn typed_malformed_collections_match_legacy_without_bypassing_limits() {
        let loader = ConfigLoader::from_values(HashMap::new(), None, None).unwrap();
        for text in ["[unfinished", "{unfinished"] {
            let sources = HashMap::from([("REVIEW_VALUE".into(), Value::String(text.into()))]);
            let mut value = Value::String("${REVIEW_VALUE}".into());
            loader.resolve_value_bounded(&mut value, &sources).unwrap();
            assert_eq!(value, Value::String(text.into()));
        }
    }
    #[test]
    fn malformed_collection_syntax_is_distinct_from_budget_failure() {
        assert!(
            matches!(preflight(b"[unfinished"), Err(ConfigError::Convert(message)) if message == MALFORMED_YAML)
        );
        assert!(preflight(format!("[{}]", "1,".repeat(RuleBudget::NODES + 1)).as_bytes()).is_err());
    }
}
