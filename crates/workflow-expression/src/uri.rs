use crate::error::runtime;
use crate::{Category, ExpressionError};
use serde_json::Value;

/// Splits URI structure before substitution; placeholders never influence authority/query/fragment.
pub fn encode_path_placeholders(uri: &str, context: &Value) -> Result<String, ExpressionError> {
    let fail = || runtime(Category::Invalid);
    if uri.contains("${") {
        return Err(fail());
    }
    let suffix_start = uri.find(['?', '#']).unwrap_or(uri.len());
    let prefix_end = if let Some(scheme) = uri[..suffix_start].find("://") {
        let s = &uri[..scheme];
        if s.is_empty()
            || !s.as_bytes()[0].is_ascii_alphabetic()
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            return Err(fail());
        }
        uri[scheme + 3..suffix_start]
            .find('/')
            .map_or(suffix_start, |i| scheme + 3 + i)
    } else if uri.starts_with("//") {
        uri[2..suffix_start]
            .find('/')
            .map_or(suffix_start, |i| 2 + i)
    } else {
        // Reject opaque schemes; relative LightAPI paths remain supported.
        if uri[..suffix_start]
            .split('/')
            .next()
            .unwrap_or("")
            .contains(':')
        {
            return Err(fail());
        }
        0
    };
    if uri[..prefix_end].contains(['{', '}']) || uri[suffix_start..].contains(['{', '}']) {
        return Err(fail());
    }
    let path = &uri[prefix_end..suffix_start];
    let mut out = uri[..prefix_end].to_owned();
    let mut i = 0;
    while i < path.len() {
        let c = path[i..].chars().next().expect("character boundary");
        if c == '}' {
            return Err(fail());
        }
        if c != '{' {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let end = path[i + 1..]
            .find('}')
            .map(|n| i + 1 + n)
            .ok_or_else(fail)?;
        let name = &path[i + 1..end];
        if name.is_empty()
            || !(name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(fail());
        }
        let value = context.get(name).ok_or_else(fail)?;
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
            _ => return Err(fail()),
        };
        if matches!(text.as_str(), "" | "." | "..") {
            return Err(fail());
        }
        for b in text.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                use std::fmt::Write;
                write!(out, "%{b:02X}").expect("String writer");
            }
        }
        i = end + 1;
    }
    out.push_str(&uri[suffix_start..]);
    Ok(out)
}

/// Static URI structure/name validation; no runtime values or substitution.
pub(crate) fn validate_path_placeholders(uri: &str) -> Result<(), ExpressionError> {
    let fail = || runtime(Category::Invalid);
    if uri.contains("${") {
        return Err(fail());
    }
    let suffix_start = uri.find(['?', '#']).unwrap_or(uri.len());
    let prefix_end = if let Some(scheme) = uri[..suffix_start].find("://") {
        let s = &uri[..scheme];
        if s.is_empty()
            || !s.as_bytes()[0].is_ascii_alphabetic()
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            return Err(fail());
        }
        uri[scheme + 3..suffix_start]
            .find('/')
            .map_or(suffix_start, |i| scheme + 3 + i)
    } else if uri.starts_with("//") {
        uri[2..suffix_start]
            .find('/')
            .map_or(suffix_start, |i| 2 + i)
    } else {
        // Reject opaque schemes; relative LightAPI paths remain supported.
        if uri[..suffix_start]
            .split('/')
            .next()
            .unwrap_or("")
            .contains(':')
        {
            return Err(fail());
        }
        0
    };
    if uri[..prefix_end].contains(['{', '}']) || uri[suffix_start..].contains(['{', '}']) {
        return Err(fail());
    }
    let path = &uri[prefix_end..suffix_start];
    let mut i = 0;
    while i < path.len() {
        let c = path[i..].chars().next().expect("character boundary");
        if c == '}' {
            return Err(fail());
        }
        if c != '{' {
            i += c.len_utf8();
            continue;
        }
        let end = path[i + 1..]
            .find('}')
            .map(|n| i + 1 + n)
            .ok_or_else(fail)?;
        let name = &path[i + 1..end];
        if name.is_empty()
            || !(name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(fail());
        }
        i = end + 1;
    }
    Ok(())
}
