//! Lossless sorted-key JSON identity. Numeric lexemes are deliberately preserved.
use anyhow::{Result, bail, ensure};
use std::collections::BTreeMap;

pub fn canonical_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut parser = Parser { input, at: 0 };
    let text = parser.value(0)?;
    parser.space();
    ensure!(parser.at == input.len(), "trailing JSON content");
    Ok(text.into_bytes())
}
struct Parser<'a> {
    input: &'a [u8],
    at: usize,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self
            .input
            .get(self.at)
            .is_some_and(|c| matches!(c, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.at += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.input.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn string(&mut self) -> Result<String> {
        self.space();
        let start = self.at;
        ensure!(
            self.input.get(self.at) == Some(&b'"'),
            "expected JSON string"
        );
        self.at += 1;
        loop {
            let c = *self
                .input
                .get(self.at)
                .ok_or_else(|| anyhow::anyhow!("unterminated string"))?;
            self.at += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    ensure!(self.at < self.input.len(), "unterminated escape");
                    self.at += 1;
                }
                _ => {}
            }
        }
        Ok(serde_json::from_slice(&self.input[start..self.at])?)
    }
    fn number(&mut self) -> Result<String> {
        let start = self.at;
        if self.input.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        match self.input.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.at += 1;
                self.digits();
            }
            _ => bail!("invalid JSON number"),
        }
        if self.input.get(self.at) == Some(&b'.') {
            self.at += 1;
            let digits = self.at;
            self.digits();
            ensure!(digits != self.at, "fraction needs digits");
        }
        if self
            .input
            .get(self.at)
            .is_some_and(|c| matches!(c, b'e' | b'E'))
        {
            self.at += 1;
            if self
                .input
                .get(self.at)
                .is_some_and(|c| matches!(c, b'+' | b'-'))
            {
                self.at += 1;
            }
            let digits = self.at;
            self.digits();
            ensure!(digits != self.at, "exponent needs digits");
        }
        Ok(std::str::from_utf8(&self.input[start..self.at])?.to_owned())
    }
    fn digits(&mut self) {
        while self.input.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
    }
    fn value(&mut self, depth: usize) -> Result<String> {
        ensure!(depth <= 128, "JSON nesting exceeds 128");
        self.space();
        match self.input.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut fields = BTreeMap::new();
                if !self.take(b'}') {
                    loop {
                        let key = self.string()?;
                        ensure!(self.take(b':'), "expected colon");
                        let value = self.value(depth + 1)?;
                        ensure!(
                            fields.insert(key, value).is_none(),
                            "duplicate JSON object key"
                        );
                        if self.take(b'}') {
                            break;
                        }
                        ensure!(self.take(b','), "expected comma");
                    }
                }
                let mut pairs = vec![];
                for (key, value) in fields {
                    pairs.push(format!("{}:{value}", serde_json::to_string(&key)?));
                }
                Ok(format!("{{{}}}", pairs.join(",")))
            }
            Some(b'[') => {
                self.at += 1;
                let mut values = vec![];
                if !self.take(b']') {
                    loop {
                        values.push(self.value(depth + 1)?);
                        if self.take(b']') {
                            break;
                        }
                        ensure!(self.take(b','), "expected comma");
                    }
                }
                Ok(format!("[{}]", values.join(",")))
            }
            Some(b'"') => Ok(serde_json::to_string(&self.string()?)?),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => {
                for literal in ["true", "false", "null"] {
                    if self.input[self.at..].starts_with(literal.as_bytes()) {
                        self.at += literal.len();
                        return Ok(literal.to_owned());
                    }
                }
                bail!("expected JSON value")
            }
        }
    }
}
