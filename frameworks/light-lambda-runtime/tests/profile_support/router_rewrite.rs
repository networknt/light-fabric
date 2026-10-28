//! Test-only immutable rewrite program. No HTTP, destination or credential ownership.
use fancy_regex::{Captures, Regex};

pub struct Program(Vec<(Regex, Regex, String)>);
fn expand(
    regex: &Regex,
    captures: &Captures<'_>,
    replacement: &str,
    limit: usize,
) -> Result<String, &'static str> {
    let mut chars = replacement.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => append(
                &mut result,
                &chars.next().ok_or("dangling escape")?.to_string(),
                limit,
            )?,
            '$' => {
                let value = if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut name = String::new();
                    loop {
                        let c = chars.next().ok_or("unterminated group")?;
                        if c == '}' {
                            break;
                        }
                        name.push(c);
                    }
                    if !regex.capture_names().any(|n| n == Some(name.as_str())) {
                        return Err("unknown named group");
                    }
                    captures.name(&name).map_or("", |m| m.as_str())
                } else {
                    let mut n = chars
                        .next()
                        .and_then(|c| c.to_digit(10))
                        .ok_or("invalid group")? as usize;
                    if n >= captures.len() {
                        return Err("unknown group");
                    }
                    while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                        let next = n
                            .checked_mul(10)
                            .and_then(|n| n.checked_add(d as usize))
                            .ok_or("group overflow")?;
                        if next >= captures.len() {
                            break;
                        }
                        chars.next();
                        n = next;
                    }
                    captures.get(n).map_or("", |m| m.as_str())
                };
                append(&mut result, value, limit)?;
            }
            _ => append(&mut result, &c.to_string(), limit)?,
        }
    }
    Ok(result)
}
fn append(out: &mut String, part: &str, limit: usize) -> Result<(), &'static str> {
    if part.len() > limit.saturating_sub(out.len()) {
        return Err("output");
    }
    out.push_str(part);
    Ok(())
}
impl Program {
    pub fn compile(rules: &[(&str, &str)]) -> Self {
        Self::try_compile(rules).unwrap()
    }
    pub fn try_compile(rules: &[(&str, &str)]) -> Result<Self, &'static str> {
        Ok(Self(
            rules
                .iter()
                .map(|(pattern, replacement)| {
                    Ok((
                        Regex::new(&format!(r"\A(?:{pattern})\z")).map_err(|_| "compile")?,
                        Regex::new(pattern).map_err(|_| "compile")?,
                        replacement.to_string(),
                    ))
                })
                .collect::<Result<_, &'static str>>()?,
        ))
    }
    pub fn rewrite(&self, path: &str) -> Result<String, &'static str> {
        self.rewrite_bounded(path, usize::MAX)
    }
    pub fn rewrite_bounded(&self, path: &str, limit: usize) -> Result<String, &'static str> {
        for (full, search, replacement) in &self.0 {
            if !full.is_match(path).map_err(|_| "engine")? {
                continue;
            }
            let mut out = String::new();
            let mut copied = 0;
            let mut pos = 0;
            while let Some(captures) = search.captures_from_pos(path, pos).map_err(|_| "engine")? {
                let m = captures.get(0).unwrap();
                append(&mut out, &path[copied..m.start()], limit)?;
                let remaining = limit.saturating_sub(out.len());
                let expanded = expand(search, &captures, replacement, remaining)?;
                append(&mut out, &expanded, limit)?;
                copied = m.end();
                pos = m.end();
                if m.start() == m.end() {
                    if pos == path.len() {
                        break;
                    }
                    pos += path[pos..].chars().next().unwrap().len_utf8();
                }
            }
            append(&mut out, &path[copied..], limit)?;
            return Ok(out);
        }
        let mut out = String::new();
        append(&mut out, path, limit)?;
        Ok(out)
    }
}
