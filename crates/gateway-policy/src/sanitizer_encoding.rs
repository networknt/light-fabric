//! Java OWASP 1.5.0 encoding, qualified separately from JSON/body admission.
use crate::sanitizer_tables::TABLES;
use std::fmt::Write;
pub fn encode(name: &str, input: &str) -> Option<String> {
    let (_, ascii, overrides) = TABLES
        .iter()
        .chain(crate::sanitizer_extra_tables::TABLES)
        .find(|(n, _, _)| *n == name)?;
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        let code = c as u32;
        if code < 256 {
            let encoded = ascii[code as usize];
            if name == "java"
                && code < 32
                && !matches!(c, '\u{8}' | '\t' | '\n' | '\u{c}' | '\r')
                && next.is_some_and(|n| ('0'..='7').contains(&n))
            {
                write!(result, "\\{code:03o}").ok()?;
            } else {
                result.push_str(encoded);
            }
            if name.starts_with("css-")
                && encoded.starts_with('\\')
                && next.is_some_and(css_escape_separator)
            {
                result.push(' ');
            }
        } else if name == "java" {
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(result, "\\u{unit:04x}").ok()?;
            }
        } else if name.starts_with("uri") {
            for b in c.encode_utf8(&mut [0; 4]).bytes() {
                write!(result, "%{b:02X}").ok()?;
            }
        } else if let Some((_, encoded)) = overrides.iter().find(|(n, _)| *n == code) {
            result.push_str(encoded);
            if name.starts_with("css-")
                && encoded.starts_with('\\')
                && next.is_some_and(css_escape_separator)
            {
                result.push(' ');
            }
        } else {
            result.push(c);
        }
    }
    Some(result)
}
fn css_escape_separator(c: char) -> bool {
    c.is_ascii_hexdigit() || matches!(c, ' ' | '\n' | '\r' | '\t' | '\u{c}')
}

/// JSON escape decoding can contain isolated UTF-16 units even though ingress
/// bytes are UTF-8. Preserve the actual OWASP handling rather than serde's stricter
/// String conversion, which would reject before the Java sanitizer runs.
pub fn encode_utf16(name: &str, input: &[u16]) -> Option<Vec<u16>> {
    TABLES
        .iter()
        .chain(crate::sanitizer_extra_tables::TABLES)
        .find(|(n, _, _)| *n == name)?;
    let mut output = Vec::new();
    let mut valid = String::new();
    let mut i = 0;
    while i < input.len() {
        let unit = input[i];
        if (0xd800..=0xdbff).contains(&unit)
            && input
                .get(i + 1)
                .is_some_and(|u| (0xdc00..=0xdfff).contains(u))
        {
            let code = 0x10000 + ((unit as u32 - 0xd800) << 10) + (input[i + 1] as u32 - 0xdc00);
            valid.push(char::from_u32(code)?);
            i += 2;
            continue;
        }
        if !(0xd800..=0xdfff).contains(&unit) {
            valid.push(char::from_u32(unit as u32)?);
            i += 1;
            continue;
        }
        output.extend(encode(name, &valid)?.encode_utf16());
        valid.clear();
        match name {
            "java"
            | "json"
            | "javascript"
            | "javascript-attribute"
            | "javascript-block"
            | "javascript-source" => output.extend(format!("\\u{unit:04x}").encode_utf16()),
            "css-string" | "css-url" => output.push(b'_' as u16),
            "uri" | "uri-component" | "html-attribute-unquoted" => output.push(b'-' as u16),
            _ => output.push(b' ' as u16),
        }
        i += 1;
    }
    output.extend(encode(name, &valid)?.encode_utf16());
    Some(output)
}
