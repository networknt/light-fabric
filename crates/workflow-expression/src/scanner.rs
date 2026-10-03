use crate::error::{Category, ScanError, admission};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Literal {
        text: String,
        offset: usize,
        end: usize,
    },
    Span {
        source: String,
        offset: usize,
        source_offset: usize,
        end: usize,
        index: usize,
    },
}
/// Lexes CEL strings (including raw/bytes/triple literals) before counting brackets.
pub fn scan(template: &str) -> Result<Vec<Segment>, ScanError> {
    let b = template.as_bytes();
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut literal_start = 0;
    let mut i = 0;
    let mut index = 0;
    while i < b.len() {
        if b[i..].starts_with(b"$${") {
            text.push_str("${");
            i += 3;
            continue;
        }
        if !b[i..].starts_with(b"${") {
            let ch = template[i..].chars().next().expect("character boundary");
            text.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if !text.is_empty() {
            segments.push(Segment::Literal {
                text: std::mem::take(&mut text),
                offset: literal_start,
                end: i,
            });
        }
        let start = i;
        i += 2;
        let source_start = i;
        let fail = |category| admission(category).at(index, start);
        if b.get(i) == Some(&b'{') {
            return Err(fail(Category::Unsupported));
        }
        let mut stack = Vec::new();
        let mut closed = false;
        while i < b.len() {
            let quote_at = if matches!(b[i], b'\'' | b'"') {
                Some((i, false))
            } else if matches!(b[i], b'r' | b'R' | b'b' | b'B') {
                let mut q = i;
                let mut raw = false;
                while q < b.len() && q < i + 2 && matches!(b[q], b'r' | b'R' | b'b' | b'B') {
                    raw |= matches!(b[q], b'r' | b'R');
                    q += 1;
                }
                if q < b.len() && matches!(b[q], b'\'' | b'"') {
                    Some((q, raw))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((q, raw)) = quote_at {
                let quote = b[q];
                let triple = b.get(q..q + 3) == Some(&[quote, quote, quote]);
                let width = if triple { 3 } else { 1 };
                i = q + width;
                let mut ended = false;
                while i < b.len() {
                    if !raw && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == quote && (!triple || b.get(i..i + 3) == Some(&[quote, quote, quote]))
                    {
                        i += width;
                        ended = true;
                        break;
                    }
                    i += 1;
                }
                if !ended {
                    return Err(fail(Category::Invalid));
                }
                continue;
            }
            // CEL comments may contain braces and quotes too.
            if b[i..].starts_with(b"//") {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            match b[i] {
                b'(' => stack.push(b')'),
                b'[' => stack.push(b']'),
                b'{' => stack.push(b'}'),
                b'}' if stack.is_empty() => {
                    closed = true;
                    break;
                }
                b')' | b']' | b'}' if stack.pop() != Some(b[i]) => {
                    return Err(fail(Category::Invalid));
                }
                _ => (),
            }
            i += 1;
        }
        if !closed {
            return Err(fail(Category::Invalid));
        }
        let source = &template[source_start..i];
        if source.trim().is_empty() {
            return Err(fail(Category::Invalid));
        }
        let source_offset = source_start;
        i += 1;
        segments.push(Segment::Span {
            source: source.to_owned(),
            offset: start,
            source_offset,
            end: i,
            index,
        });
        index += 1;
        literal_start = i;
    }
    if !text.is_empty() || segments.is_empty() {
        segments.push(Segment::Literal {
            text,
            offset: literal_start,
            end: i,
        });
    }
    Ok(segments)
}
