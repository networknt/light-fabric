//! Approved WP12 Java-compatible base-path profile and bounded normalization.
//! No provider, network, clock or invocation-state operations.
use super::{SourceFormat, ordered_source};
use regex::{Regex, RegexBuilder};
use serde_json::Value;
pub const PROFILE_ERROR: &str = "specification: base-path normalization profile rejected";
pub const MAX_NORMALIZATION_PATH_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizationInputTooLarge;
const PATTERN_BYTES: usize = 1024;
const NODES: usize = 256;
const DEPTH: usize = 16;
const REPEAT: u32 = 64;
const TRANSLATED_BYTES: usize = 16384;
const PROGRAM_BYTES: usize = 1048576;
const CACHE_BYTES: usize = 1048576;
const ENGINE_DEPTH: u32 = 64;
type R<T> = Result<T, &'static str>;
type Set = Vec<(u32, u32)>;
#[derive(Clone, Copy, Default)]
struct Flags {
    i: bool,
    s: bool,
}
struct Parser {
    c: Vec<char>,
    at: usize,
    nodes: usize,
    capture_groups: bool,
}
fn domain() -> Set {
    vec![(0, 0xd7ff), (0xe000, 0x10ffff)]
}
fn union(mut ranges: Set) -> Set {
    ranges.sort_unstable();
    let mut result: Set = Vec::new();
    for (a, b) in ranges {
        if let Some(last) = result.last_mut()
            && a <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(b);
            continue;
        }
        result.push((a, b));
    }
    result
}
fn intersection(a: &Set, b: &Set) -> Set {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if lo <= hi {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}
fn complement(a: &Set) -> Set {
    let mut out = Vec::new();
    for (lo, hi) in domain() {
        let mut next = lo;
        for &(a, b) in a {
            if b < next || a > hi {
                continue;
            }
            if a > next {
                out.push((next, a - 1));
            }
            next = next.max(b.saturating_add(1));
            if next > hi {
                break;
            }
        }
        if next <= hi {
            out.push((next, hi));
        }
    }
    out
}
fn folded(mut set: Set, flags: Flags) -> Set {
    set = union(set);
    if flags.i {
        for lower in b'a'..=b'z' {
            let upper = lower.to_ascii_uppercase();
            if set.iter().any(|&(a, b)| {
                (a <= lower as u32 && lower as u32 <= b) || (a <= upper as u32 && upper as u32 <= b)
            }) {
                set.push((lower as u32, lower as u32));
                set.push((upper as u32, upper as u32));
            }
        }
    }
    intersection(&union(set), &domain())
}
fn render_set(set: &Set) -> String {
    if set.is_empty() {
        return r"(?-u:\b\B)".into();
    }
    let mut out = String::from("[");
    for &(a, b) in set {
        out.push_str(&format!("\\x{{{a:x}}}"));
        if a != b {
            out.push_str(&format!("-\\x{{{b:x}}}"));
        }
    }
    out.push(']');
    out
}
fn shorthand(c: char) -> Option<Set> {
    let lower = c.to_ascii_lowercase();
    let base = match lower {
        'd' => vec![(48, 57)],
        'w' => vec![(48, 57), (65, 90), (95, 95), (97, 122)],
        's' => vec![(9, 13), (32, 32)],
        _ => return None,
    };
    Some(if c.is_ascii_uppercase() {
        complement(&base)
    } else {
        base
    })
}
fn escaped_literal(c: char) -> Option<char> {
    match c {
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'f' => Some('\u{c}'),
        c if c.is_ascii() && (' '..='~').contains(&c) && !c.is_ascii_alphanumeric() => Some(c),
        _ => None,
    }
}
fn literal(c: char, f: Flags) -> String {
    if f.i && c.is_ascii_alphabetic() {
        render_set(&folded(vec![(c as u32, c as u32)], f))
    } else {
        regex::escape(&c.to_string())
    }
}
impl Parser {
    fn bump(&mut self) -> R<()> {
        self.nodes += 1;
        if self.nodes > NODES {
            Err("node-limit")
        } else {
            Ok(())
        }
    }
    fn peek(&self) -> Option<char> {
        self.c.get(self.at).copied()
    }
    fn take(&mut self) -> R<char> {
        let c = self.peek().ok_or("malformed")?;
        self.at += 1;
        Ok(c)
    }
    fn bounded(&self, s: &str) -> R<()> {
        if s.len() > TRANSLATED_BYTES {
            Err("translation-limit")
        } else {
            Ok(())
        }
    }
    fn class_piece(&mut self) -> R<(Set, Option<u32>)> {
        let c = self.take()?;
        if c == '[' {
            return Err("unsupported");
        }
        if c == '\\' {
            let c = self.take()?;
            if let Some(s) = shorthand(c) {
                return Ok((s, None));
            }
            let c = escaped_literal(c).ok_or("unsupported")?;
            return Ok((vec![(c as u32, c as u32)], Some(c as u32)));
        }
        Ok((vec![(c as u32, c as u32)], Some(c as u32)))
    }
    fn class(&mut self, depth: usize, f: Flags) -> R<Set> {
        if depth > DEPTH {
            return Err("depth-limit");
        }
        self.bump()?;
        let negative = self.peek() == Some('^');
        if negative {
            self.at += 1;
        }
        let mut out: Set = Vec::new();
        let mut members = 0;
        loop {
            if self.peek() == Some(']') {
                break;
            }
            if self.peek().is_none() {
                return Err("malformed");
            }
            if self.peek() == Some('&') && self.c.get(self.at + 1) == Some(&'&') {
                break;
            }
            let (mut set, scalar) = self.class_piece()?;
            members += 1;
            if self.peek() == Some('-') && self.c.get(self.at + 1) != Some(&']') {
                self.at += 1;
                let (_, end) = self.class_piece()?;
                let (a, b) = (scalar.ok_or("unsupported")?, end.ok_or("unsupported")?);
                if a > b {
                    return Err("malformed");
                }
                set = vec![(a, b)];
            }
            out.extend(set);
        }
        if members == 0 {
            return Err("malformed");
        }
        out = folded(out, f);
        while self.peek() == Some('&') && self.c.get(self.at + 1) == Some(&'&') {
            self.at += 2;
            if self.take()? != '[' {
                return Err("unsupported");
            }
            let rhs = self.class(depth + 1, f)?;
            out = intersection(&out, &rhs);
        }
        if self.take()? != ']' {
            return Err("unsupported");
        }
        Ok(if negative { complement(&out) } else { out })
    }
    fn count(&mut self) -> R<u32> {
        let begin = self.at;
        let mut n = 0u32;
        while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
            self.at += 1;
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(c as u32 - '0' as u32))
                .ok_or("repeat-limit")?;
            if n > REPEAT {
                return Err("repeat-limit");
            }
        }
        if self.at == begin {
            return Err("malformed");
        }
        Ok(n)
    }
    fn quantifier(&mut self) -> R<String> {
        self.bump()?;
        let c = self.take()?;
        let mut out = if c == '{' {
            let min = self.count()?;
            match self.take()? {
                '}' => format!("{{{min}}}"),
                ',' => {
                    let max = if self.peek() == Some('}') {
                        None
                    } else {
                        Some(self.count()?)
                    };
                    if self.take()? != '}' || max.is_some_and(|m| m < min) {
                        return Err("malformed");
                    }
                    match max {
                        Some(max) => format!("{{{min},{max}}}"),
                        None => format!("{{{min},}}"),
                    }
                }
                _ => return Err("malformed"),
            }
        } else {
            c.to_string()
        };
        if self.peek() == Some('?') {
            self.at += 1;
            out.push('?');
        }
        if self.peek() == Some('+') {
            return Err("unsupported");
        }
        Ok(out)
    }
    fn expression(&mut self, depth: usize, f: &mut Flags, group: bool) -> R<String> {
        if depth > DEPTH {
            return Err("depth-limit");
        }
        let mut out = String::new();
        let mut repeatable = false;
        while let Some(c) = self.peek() {
            match c {
                ')' => {
                    if !group {
                        return Err("malformed");
                    }
                    self.at += 1;
                    return Ok(out);
                }
                '|' => {
                    self.bump()?;
                    self.at += 1;
                    out.push('|');
                    repeatable = false;
                }
                '*' | '+' | '?' | '{' => {
                    if !repeatable {
                        return Err("unsupported");
                    }
                    out.push_str(&self.quantifier()?);
                    repeatable = false;
                }
                '$' => return Err("unsupported"),
                '^' => {
                    self.bump()?;
                    self.at += 1;
                    out.push_str(r"\A");
                    repeatable = false;
                }
                '.' => {
                    self.bump()?;
                    self.at += 1;
                    let set = if f.s {
                        domain()
                    } else {
                        complement(&vec![(10, 10), (13, 13), (0x85, 0x85), (0x2028, 0x2029)])
                    };
                    out.push_str(&render_set(&set));
                    repeatable = true;
                }
                '[' => {
                    self.at += 1;
                    out.push_str(&render_set(&self.class(depth + 1, *f)?));
                    repeatable = true;
                }
                '(' => {
                    self.bump()?;
                    self.at += 1;
                    let mut inner = *f;
                    let mut capture = self.capture_groups;
                    let mut name = None;
                    if self.peek() == Some('?') {
                        self.at += 1;
                        if self.peek() == Some(':') {
                            self.at += 1;
                            capture = false;
                        } else if self.capture_groups && self.peek() == Some('<') {
                            self.at += 1;
                            let mut value = String::new();
                            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                                value.push(self.take()?);
                            }
                            if value.is_empty()
                                || !value.starts_with(|c: char| c.is_ascii_alphabetic())
                                || self.take()? != '>'
                            {
                                return Err("unsupported");
                            }
                            name = Some(value);
                        } else {
                            capture = false;
                            let mut disabling = false;
                            let mut seen = false;
                            loop {
                                match self.take()? {
                                    'i' => {
                                        inner.i = !disabling;
                                        seen = true;
                                    }
                                    's' => {
                                        inner.s = !disabling;
                                        seen = true;
                                    }
                                    '-' if !disabling => disabling = true,
                                    ':' if seen => break,
                                    ')' if seen => {
                                        *f = inner;
                                        repeatable = false;
                                        break;
                                    }
                                    _ => return Err("unsupported"),
                                }
                            }
                            if self.c.get(self.at - 1) == Some(&')') {
                                continue;
                            }
                        }
                    }
                    let body = self.expression(depth + 1, &mut inner, true)?;
                    if let Some(name) = name {
                        out.push_str("(?P<");
                        out.push_str(&name);
                        out.push('>');
                    } else if capture {
                        out.push('(');
                    } else {
                        out.push_str("(?:");
                    }
                    out.push_str(&body);
                    out.push(')');
                    repeatable = true;
                }
                '\\' => {
                    self.at += 1;
                    let e = self.take()?;
                    if e == 'Q' {
                        self.bump()?;
                        repeatable = false;
                        while let Some(q) = self.peek() {
                            if q == '\\' && self.c.get(self.at + 1) == Some(&'E') {
                                self.at += 2;
                                break;
                            }
                            self.at += 1;
                            self.bump()?;
                            out.push_str(&literal(q, *f));
                            repeatable = true;
                            self.bounded(&out)?;
                        }
                    } else {
                        self.bump()?;
                        if let Some(set) = shorthand(e) {
                            out.push_str(&render_set(&folded(set, *f)));
                            repeatable = true;
                        } else {
                            match e {
                                'A' | 'G' => {
                                    out.push_str(r"\A");
                                    repeatable = false;
                                }
                                'z' => {
                                    out.push_str(r"\z");
                                    repeatable = false;
                                }
                                'b' | 'B' => {
                                    out.push_str(&format!("(?-u:\\{e})"));
                                    repeatable = false;
                                }
                                _ => {
                                    out.push_str(&literal(
                                        escaped_literal(e).ok_or("unsupported")?,
                                        *f,
                                    ));
                                    repeatable = true;
                                }
                            }
                        }
                    }
                }
                _ => {
                    self.bump()?;
                    self.at += 1;
                    out.push_str(&literal(c, *f));
                    repeatable = true;
                }
            }
            self.bounded(&out)?;
        }
        if group { Err("malformed") } else { Ok(out) }
    }
}
fn compile(pattern: &str) -> R<(Regex, String, usize)> {
    compile_mode(pattern, false)
}
/// Opt-in qualification API only. The inherited Specification subset/limits
/// are NOT an admitted Router compatibility profile. Unsupported constructs
/// return an error, never silently lose captures or fall through as literals.
pub fn compile_router_capture_candidate(pattern: &str) -> R<Regex> {
    compile_mode(pattern, true).map(|(regex, _, _)| regex)
}
fn compile_mode(pattern: &str, capture_groups: bool) -> R<(Regex, String, usize)> {
    if pattern.len() > PATTERN_BYTES {
        return Err("pattern-limit");
    }
    let mut parser = Parser {
        c: pattern.chars().collect(),
        at: 0,
        nodes: 0,
        capture_groups,
    };
    let translated = parser.expression(0, &mut Flags::default(), false)?;
    let re = RegexBuilder::new(&translated)
        .unicode(true)
        .case_insensitive(false)
        .multi_line(false)
        .dot_matches_new_line(false)
        .ignore_whitespace(false)
        .swap_greed(false)
        .crlf(false)
        .size_limit(PROGRAM_BYTES)
        .dfa_size_limit(CACHE_BYTES)
        .nest_limit(ENGINE_DEPTH)
        .build()
        .map_err(|error| match error {
            regex::Error::CompiledTooBig(_) => "program-limit",
            _ => "compiler-rejected",
        })?;
    Ok((re, translated, parser.nodes))
}

/// Immutable, compiled configuration with an independently bounded pure entry.
#[derive(Clone)]
pub struct BasePathProfile {
    _compiled: Regex,
}
impl BasePathProfile {
    /// First unanchored Java-compatible removal on an already admitted path.
    /// The independent guard also prevents direct callers bypassing admission.
    /// Clone only after admission: regex-automata gives this search a fresh cache
    /// pool while sharing immutable compiled data. Drop the pool on return.
    pub fn normalize_bounded_path(&self, path: &str) -> Result<String, NormalizationInputTooLarge> {
        if path.len() > MAX_NORMALIZATION_PATH_BYTES {
            return Err(NormalizationInputTooLarge);
        }
        let worker_regex = self._compiled.clone();
        let (prefix, suffix) = match worker_regex.find(path) {
            Some(found) => (&path[..found.start()], &path[found.end()..]),
            None => (path, ""),
        };
        let slash = !prefix
            .chars()
            .chain(suffix.chars())
            .next()
            .is_some_and(|c| c == '/');
        let mut output = String::with_capacity(prefix.len() + suffix.len() + usize::from(slash));
        if slash {
            output.push('/');
        }
        output.push_str(prefix);
        output.push_str(suffix);
        Ok(output)
    }
    pub fn from_pattern(pattern: &str) -> Result<Self, &'static str> {
        compile(pattern)
            .map(|(regex, _, _)| Self { _compiled: regex })
            .map_err(|_| PROFILE_ERROR)
    }
    /// Same metadata tree budgets without confusing expanded JSON encoding
    /// length with the bounded original source capture length.
    pub fn from_metadata_projection(
        metadata_json: &[u8],
        fallback: Option<&Value>,
    ) -> Result<Self, &'static str> {
        let (document, _) = ordered_source::parse_projection(metadata_json)?;
        let pattern = effective(&document, fallback).map_err(|_| PROFILE_ERROR)?;
        Self::from_pattern(pattern)
    }
    /// Trusted raw source, using the existing bounded ordered loader. Fallback
    /// type validation applies even when its regex is not selected.
    pub fn from_source(
        source: &[u8],
        format: SourceFormat,
        fallback: Option<&Value>,
    ) -> Result<Self, &'static str> {
        let (document, _) = ordered_source::parse(source, format)?;
        let pattern = effective(&document, fallback).map_err(|_| PROFILE_ERROR)?;
        Self::from_pattern(pattern)
    }
}
fn effective<'a>(document: &'a Value, fallback: Option<&'a Value>) -> R<&'a str> {
    let fallback = match fallback {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s,
        _ => return Err("malformed-source"),
    };
    let servers = match document.get("servers") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(a)) => a.as_slice(),
        _ => return Err("malformed-source"),
    };
    let first = servers.first();
    if first.is_some_and(|server| !server.is_object()) {
        return Err("malformed-source");
    }
    let url = match first.and_then(|s| s.get("url")) {
        None => "",
        Some(Value::String(s)) => s,
        _ => return Err("malformed-source"),
    };
    let derived = if let Some(at) = url.find("://") {
        let start = at + 3;
        url[start..]
            .find('/')
            .map(|n| &url[start + n..])
            .unwrap_or("")
    } else if url.starts_with('/') {
        url
    } else {
        ""
    };
    Ok(if derived.is_empty() {
        fallback
    } else {
        derived
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn opt_in_captures_preserve_default_specification_translation() {
        let original = super::compile("/(x)(?:y)(?<name>z)");
        assert!(
            original.is_err(),
            "Specification does not gain named-group syntax"
        );
        let original = super::compile("/(x)(?:y)").unwrap().0;
        assert_eq!(original.captures_len(), 1);
        let candidate = super::compile_router_capture_candidate("/(x)(?:y)(?<name>z)").unwrap();
        let captures = candidate.captures("/xyz").unwrap();
        assert_eq!(&captures[1], "x");
        assert_eq!(&captures["name"], "z");
        // Never force Java lookaround/backreferences/large repeats through the
        // accepted Specification subset or call these exclusions approved.
        for pattern in ["/(?=x)x", r"/(x)\1", "/x{65}"] {
            assert!(super::compile_router_capture_candidate(pattern).is_err());
        }
    }
    use super::*;
    fn normal(re: &Regex, path: &str) -> String {
        let out = match re.find(path) {
            Some(m) => format!("{}{}", &path[..m.start()], &path[m.end()..]),
            None => path.to_owned(),
        };
        if out.starts_with('/') {
            out
        } else {
            format!("/{out}")
        }
    }
    #[test]
    fn approved_java_matrix_392() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/data/wp12-basepath-profile.json"))
                .unwrap();
        let java: Vec<Value> = include_str!("../../tests/data/wp12-basepath-java.jsonl")
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(cases.len(), 392);
        for case in cases {
            let id = case["id"].as_u64().unwrap() as usize;
            let source;
            let pattern = if let Some(pattern) = case.get("pattern").and_then(Value::as_str) {
                Ok(pattern)
            } else {
                let bytes = if let Some(yaml) = case.get("sourceYaml").and_then(Value::as_str) {
                    yaml.as_bytes().to_vec()
                } else {
                    serde_json::to_vec(&case["source"]).unwrap()
                };
                let format = if case["format"] == "yaml" {
                    SourceFormat::Yaml
                } else {
                    SourceFormat::Json
                };
                source = ordered_source::parse(&bytes, format).unwrap().0;
                effective(&source, case.get("fallback"))
            };
            let compiled = pattern.and_then(compile);
            if case["expectAdmitted"] == true {
                let (re, _, _) = compiled.unwrap_or_else(|error| panic!("case {id}: {error}"));
                let actual = normal(&re, case["input"].as_str().unwrap());
                use sha2::{Digest, Sha256};
                assert_eq!(
                    actual.len(),
                    java[id]["bytes"].as_u64().unwrap() as usize,
                    "case {id}"
                );
                assert_eq!(
                    format!("{:x}", Sha256::digest(actual.as_bytes())),
                    java[id]["sha256"].as_str().unwrap(),
                    "case {id}"
                );
            } else {
                assert_eq!(
                    compiled.err().unwrap(),
                    case["expectReason"].as_str().unwrap(),
                    "case {id}"
                );
                if let Some(p) = case.get("pattern").and_then(Value::as_str) {
                    assert_eq!(BasePathProfile::from_pattern(p).err(), Some(PROFILE_ERROR));
                }
            }
            if case.get("pattern").is_none() {
                let bytes = if let Some(yaml) = case.get("sourceYaml").and_then(Value::as_str) {
                    yaml.as_bytes().to_vec()
                } else {
                    serde_json::to_vec(&case["source"]).unwrap()
                };
                let format = if case["format"] == "yaml" {
                    SourceFormat::Yaml
                } else {
                    SourceFormat::Json
                };
                let actual = BasePathProfile::from_source(&bytes, format, case.get("fallback"));
                assert_eq!(
                    actual.is_ok(),
                    case["expectAdmitted"] == true,
                    "source case {id}"
                );
                if actual.is_err() {
                    assert_eq!(actual.err(), Some(PROFILE_ERROR));
                }
            }
        }
    }
    #[test]
    fn source_fallback_types_and_unused_regex() {
        let source = br#"{"servers":[{"url":"/api"}]}"#;
        assert!(
            BasePathProfile::from_source(
                source,
                SourceFormat::Json,
                Some(&Value::String("(?=secret)".into()))
            )
            .is_ok()
        );
        assert_eq!(
            BasePathProfile::from_source(source, SourceFormat::Json, Some(&Value::Bool(false)))
                .err(),
            Some(PROFILE_ERROR)
        );
        assert!(BasePathProfile::from_source(br#"{}"#, SourceFormat::Json, None).is_ok());
        for source in [
            br#"{"servers":42}"#.as_slice(),
            br#"{"servers":[42]}"#,
            br#"{"servers":[{"url":42}]}"#,
        ] {
            assert_eq!(
                BasePathProfile::from_source(source, SourceFormat::Json, None).err(),
                Some(PROFILE_ERROR)
            );
        }
        assert_eq!(
            BasePathProfile::from_source(
                br#"{"servers":[{"url":null}]}"#,
                SourceFormat::Json,
                None
            )
            .err(),
            Some(PROFILE_ERROR)
        );
        let over = vec![b' '; super::super::MAX_SOURCE_BYTES + 1];
        assert!(BasePathProfile::from_source(&over, SourceFormat::Json, None).is_err());
    }
    #[test]
    fn bounded_entry_preserves_matrix_and_explicit_runtime_amendment() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/data/wp12-basepath-profile.json"))
                .unwrap();
        let java: Vec<Value> = include_str!("../../tests/data/wp12-basepath-java.jsonl")
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut normalizations = 0;
        let mut oversize = 0;
        for case in cases.iter().filter(|c| c["expectAdmitted"] == true) {
            let profile = if let Some(pattern) = case.get("pattern").and_then(Value::as_str) {
                BasePathProfile::from_pattern(pattern).unwrap()
            } else {
                let bytes = if let Some(yaml) = case.get("sourceYaml").and_then(Value::as_str) {
                    yaml.as_bytes().to_vec()
                } else {
                    serde_json::to_vec(&case["source"]).unwrap()
                };
                BasePathProfile::from_source(
                    &bytes,
                    if case["format"] == "yaml" {
                        SourceFormat::Yaml
                    } else {
                        SourceFormat::Json
                    },
                    case.get("fallback"),
                )
                .unwrap()
            };
            let path = case["input"].as_str().unwrap();
            if path.len() > MAX_NORMALIZATION_PATH_BYTES {
                assert_eq!(case["id"], 309);
                assert_eq!(
                    profile.normalize_bounded_path(path),
                    Err(NormalizationInputTooLarge)
                );
                oversize += 1;
            } else {
                let actual = profile.normalize_bounded_path(path).unwrap();
                use sha2::{Digest, Sha256};
                let id = case["id"].as_u64().unwrap() as usize;
                assert_eq!(
                    format!("{:x}", Sha256::digest(actual.as_bytes())),
                    java[id]["sha256"].as_str().unwrap(),
                    "case {id}"
                );
                normalizations += 1;
            }
        }
        assert_eq!((normalizations, oversize), (318, 1));
        let profile = BasePathProfile::from_pattern("never-match").unwrap();
        for path in ["x".repeat(8192), "é".repeat(4096)] {
            assert_eq!(path.len(), 8192);
            let output = profile.normalize_bounded_path(&path).unwrap();
            assert_eq!(output.len(), 8193);
            assert_eq!(output, format!("/{path}"));
            assert_eq!(
                profile.normalize_bounded_path(&(path + "x")),
                Err(NormalizationInputTooLarge)
            );
        }
    }
}
