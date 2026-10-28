//! Ordered JSON body sanitization. Iterative parsing/walking/serialization avoids
//! imposing a Rust stack-depth cutoff or changing global serde map ordering.
use super::sanitizer_encoding::encode_utf16;
use std::fmt::Write;
#[derive(Clone)]
enum Node {
    Object(Vec<(Vec<u16>, usize)>),
    Array(Vec<usize>),
    String(Vec<u16>),
    Number(String),
    Literal(&'static str),
}
struct Frame {
    node: usize,
    stage: u8,
    key: Option<Vec<u16>>,
}
struct Parser<'a> {
    input: &'a [u8],
    at: usize,
    nodes: Vec<Node>,
}
impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .input
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }
    fn string(&mut self) -> Result<Vec<u16>, &'static str> {
        if self.input.get(self.at) != Some(&b'"') {
            return Err("JSON string required");
        }
        self.at += 1;
        let mut out = Vec::new();
        let mut start = self.at;
        loop {
            let b = *self.input.get(self.at).ok_or("unterminated JSON string")?;
            if b == b'"' || b == b'\\' {
                out.extend(
                    std::str::from_utf8(&self.input[start..self.at])
                        .map_err(|_| "JSON UTF-8 required")?
                        .encode_utf16(),
                );
                self.at += 1;
                if b == b'"' {
                    return Ok(out);
                }
                let escape = *self.input.get(self.at).ok_or("JSON escape required")?;
                self.at += 1;
                match escape {
                    b'"' => out.push(34),
                    b'\\' => out.push(92),
                    b'/' => out.push(47),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(10),
                    b'r' => out.push(13),
                    b't' => out.push(9),
                    b'u' => {
                        let end = self.at.checked_add(4).ok_or("JSON escape")?;
                        let raw = self.input.get(self.at..end).ok_or("JSON escape")?;
                        if !raw.iter().all(u8::is_ascii_hexdigit) {
                            return Err("JSON escape");
                        }
                        let raw = std::str::from_utf8(raw).map_err(|_| "JSON escape")?;
                        let unit = u16::from_str_radix(raw, 16).map_err(|_| "JSON escape")?;
                        out.push(unit);
                        self.at = end;
                    }
                    _ => return Err("JSON escape required"),
                };
                start = self.at;
            } else {
                if b < 32 {
                    return Err("JSON control character");
                }
                self.at += 1;
            }
        }
    }
    fn value(&mut self) -> Result<(usize, bool), &'static str> {
        self.whitespace();
        let b = *self.input.get(self.at).ok_or("JSON value required")?;
        let (node, container) = match b {
            b'{' => {
                self.at += 1;
                (Node::Object(Vec::new()), true)
            }
            b'[' => {
                self.at += 1;
                (Node::Array(Vec::new()), true)
            }
            b'"' => (Node::String(self.string()?), false),
            b't' | b'f' | b'n' => {
                let word = match b {
                    b't' => "true",
                    b'f' => "false",
                    _ => "null",
                };
                if self.input.get(self.at..self.at + word.len()) != Some(word.as_bytes()) {
                    return Err("JSON literal");
                }
                self.at += word.len();
                (Node::Literal(word), false)
            }
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                if b == b'-' {
                    self.at += 1;
                }
                match self.input.get(self.at) {
                    Some(b'0') => self.at += 1,
                    Some(b'1'..=b'9') => {
                        while self.input.get(self.at).is_some_and(u8::is_ascii_digit) {
                            self.at += 1;
                        }
                    }
                    _ => return Err("JSON number"),
                }
                if self.input.get(self.at) == Some(&b'.') {
                    self.at += 1;
                    let digits = self.at;
                    while self.input.get(self.at).is_some_and(u8::is_ascii_digit) {
                        self.at += 1;
                    }
                    if self.at == digits {
                        return Err("JSON fraction");
                    }
                }
                if self
                    .input
                    .get(self.at)
                    .is_some_and(|b| matches!(b, b'e' | b'E'))
                {
                    self.at += 1;
                    if self
                        .input
                        .get(self.at)
                        .is_some_and(|b| matches!(b, b'+' | b'-'))
                    {
                        self.at += 1;
                    }
                    let digits = self.at;
                    while self.input.get(self.at).is_some_and(u8::is_ascii_digit) {
                        self.at += 1;
                    }
                    if self.at == digits {
                        return Err("JSON exponent");
                    }
                }
                (
                    Node::Number(
                        std::str::from_utf8(&self.input[start..self.at])
                            .map_err(|_| "JSON number")?
                            .to_owned(),
                    ),
                    false,
                )
            }
            _ => return Err("JSON value required"),
        };
        let index = self.nodes.len();
        self.nodes.push(node);
        Ok((index, container))
    }
    fn parse(mut self) -> Result<Vec<Node>, &'static str> {
        let (root, container) = self.value()?;
        if !container {
            return Err("JSON object or array required");
        }
        let mut stack = vec![Frame {
            node: root,
            stage: 0,
            key: None,
        }];
        while let Some(frame) = stack.last_mut() {
            self.whitespace();
            let next = *self
                .input
                .get(self.at)
                .ok_or("unterminated JSON container")?;
            match &mut self.nodes[frame.node] {
                Node::Object(_) => match frame.stage {
                    0 | 4 => {
                        if next == b'}' && frame.stage == 0 {
                            self.at += 1;
                            stack.pop();
                            continue;
                        }
                        frame.key = Some(self.string()?);
                        frame.stage = 1;
                        continue;
                    }
                    1 => {
                        if next != b':' {
                            return Err("JSON colon required");
                        }
                        self.at += 1;
                        frame.stage = 2;
                        continue;
                    }
                    3 => {
                        if next == b'}' {
                            self.at += 1;
                            stack.pop();
                            continue;
                        }
                        if next != b',' {
                            return Err("JSON comma required");
                        }
                        self.at += 1;
                        frame.stage = 4;
                        continue;
                    }
                    _ => {}
                },
                Node::Array(_) => {
                    if frame.stage == 0 && next == b']' {
                        self.at += 1;
                        stack.pop();
                        continue;
                    }
                    if frame.stage == 3 {
                        if next == b']' {
                            self.at += 1;
                            stack.pop();
                            continue;
                        }
                        if next != b',' {
                            return Err("JSON comma required");
                        }
                        self.at += 1;
                        frame.stage = 4;
                        continue;
                    }
                }
                _ => return Err("JSON container required"),
            }
            let parent = frame.node;
            let key = frame.key.take();
            frame.stage = 3;
            let (index, container) = self.value()?;
            match &mut self.nodes[parent] {
                Node::Object(entries) => {
                    let key = key.ok_or("JSON key required")?;
                    if let Some((_, value)) = entries.iter_mut().find(|(k, _)| *k == key) {
                        *value = index;
                    } else {
                        entries.push((key, index));
                    }
                }
                Node::Array(items) => items.push(index),
                _ => return Err("JSON container required"),
            }
            if container {
                // Jackson StreamReadConstraints defaults used by the preserved
                // Java oracle: root container counts as depth one.
                if stack.len() >= 1000 {
                    return Err("JSON nesting depth");
                }
                stack.push(Frame {
                    node: index,
                    stage: 0,
                    key: None,
                });
            }
        }
        self.whitespace();
        if self.at != self.input.len() {
            return Err("trailing JSON value");
        }
        Ok(self.nodes)
    }
}
fn json_string(units: &[u16], output: &mut String) {
    output.push('"');
    for scalar in char::decode_utf16(units.iter().copied()) {
        let c = scalar.unwrap_or('?');
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            c if (c as u32) < 32 => {
                write!(output, "\\u{:04X}", c as u32).expect("String write");
            }
            _ => output.push(c),
        }
    }
    output.push('"');
}
fn java_number(raw: &str) -> Result<String, &'static str> {
    // Jackson excludes the leading minus from its number-length constraint.
    if raw.trim_start_matches('-').len() > 1000 {
        return Err("JSON number length");
    }
    if !raw.contains(['.', 'e', 'E']) {
        return Ok(if raw == "-0" { "0".into() } else { raw.into() });
    }
    let f = raw.parse::<f64>().map_err(|_| "JSON number")?;
    if !f.is_finite() {
        return Ok(if f.is_sign_negative() {
            "\"-Infinity\"".into()
        } else {
            "\"Infinity\"".into()
        });
    }
    if f == 0.0 {
        return Ok(if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        });
    }
    let mut text = serde_json::Number::from_f64(f)
        .ok_or("JSON number")?
        .to_string();
    if !(0.001..10000000.0).contains(&f.abs()) {
        let mantissa = text.split(['e', 'E']).next().unwrap_or(&text);
        if mantissa
            .chars()
            .filter(|c| c.is_ascii_digit() && *c != '0')
            .count()
            == 1
            && mantissa.chars().filter(char::is_ascii_digit).count() <= 2
        {
            text = format!("{f:.1e}");
        }
    }
    let text = text.trim_start_matches('-');
    let (base, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text, 0), |(b, e)| (b, e.parse::<i32>().unwrap_or(0)));
    let mut point = base.find('.').unwrap_or(base.len()) as i32 + exponent;
    let mut digits = base.replace('.', "");
    while digits.starts_with('0') {
        digits.remove(0);
        point -= 1;
    }
    while digits.ends_with('0') {
        digits.pop();
    }
    let mut result = if f.is_sign_negative() {
        "-".to_owned()
    } else {
        String::new()
    };
    if !(0.001..10000000.0).contains(&f.abs()) {
        result.push(digits.as_bytes()[0] as char);
        result.push('.');
        result.push_str(if digits.len() == 1 { "0" } else { &digits[1..] });
        write!(result, "E{}", point - 1).map_err(|_| "JSON number")?;
    } else if point <= 0 {
        result.push_str("0.");
        result.push_str(&"0".repeat((-point) as usize));
        result.push_str(&digits);
    } else if point as usize >= digits.len() {
        result.push_str(&digits);
        result.push_str(&"0".repeat(point as usize - digits.len()));
        result.push_str(".0");
    } else {
        result.push_str(&digits[..point as usize]);
        result.push('.');
        result.push_str(&digits[point as usize..]);
    }
    Ok(result)
}
pub fn sanitize(
    input: &[u8],
    encoder: &str,
    ignore: &[String],
    appreciate: &[String],
) -> Result<Vec<u8>, &'static str> {
    let mut nodes = Parser {
        input,
        at: 0,
        nodes: Vec::new(),
    }
    .parse()?;
    // JsonMapper.string2List uses List<Map<String,Object>>, unlike nested
    // EncoderWrapper lists. Preserve its root-array coercion boundary.
    if let Node::Array(items) = &nodes[0]
        && items
            .iter()
            .any(|i| !matches!(&nodes[*i], Node::Object(_) | Node::Literal("null")))
    {
        return Err("JSON object array required");
    }
    let ignored = ignore
        .iter()
        .map(|s| s.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let appreciated = appreciate
        .iter()
        .map(|s| s.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut pending = vec![0];
    while let Some(node) = pending.pop() {
        match &mut nodes[node] {
            Node::String(s) => *s = encode_utf16(encoder, s).ok_or("unknown encoder")?,
            Node::Array(items) => pending.extend(items.iter().copied()),
            Node::Object(items) => {
                for (key, index) in items {
                    if !ignored.contains(key)
                        && (appreciated.is_empty() || appreciated.contains(key))
                    {
                        pending.push(*index);
                    }
                }
            }
            _ => {}
        }
    }
    enum Part<'a> {
        Node(usize),
        String(&'a [u16]),
        Text(&'static str),
    }
    let mut pending = vec![Part::Node(0)];
    let mut result = String::new();
    while let Some(part) = pending.pop() {
        match part {
            Part::Text(s) => result.push_str(s),
            Part::String(s) => json_string(s, &mut result),
            Part::Node(n) => match &nodes[n] {
                Node::String(s) => json_string(s, &mut result),
                Node::Number(s) => result.push_str(&java_number(s)?),
                Node::Literal(s) => result.push_str(s),
                Node::Array(items) => {
                    result.push('[');
                    pending.push(Part::Text("]"));
                    for (i, node) in items.iter().enumerate().rev() {
                        if i + 1 < items.len() {
                            pending.push(Part::Text(","));
                        }
                        pending.push(Part::Node(*node));
                    }
                }
                Node::Object(items) => {
                    let items = items
                        .iter()
                        .filter(|(_, node)| !matches!(nodes[*node], Node::Literal("null")))
                        .collect::<Vec<_>>();
                    result.push('{');
                    pending.push(Part::Text("}"));
                    for (i, (key, node)) in items.iter().enumerate().rev() {
                        if i + 1 < items.len() {
                            pending.push(Part::Text(","));
                        }
                        pending.push(Part::Node(*node));
                        pending.push(Part::Text(":"));
                        pending.push(Part::String(key));
                    }
                }
            },
        }
    }
    Ok(result.into_bytes())
}
