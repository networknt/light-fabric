//! Local source parser with independent byte, nesting and expanded-node bounds.
//! Object order is captured during deserialization, before JSON Value conversion.
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Number, Value};
use std::{collections::BTreeSet, fmt};

pub const MAX_SOURCE_BYTES: usize = 1_048_576;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 65_536;
const MAX_EXPANDED_STRING_BYTES: usize = MAX_SOURCE_BYTES;

#[derive(Clone, Copy, Debug)]
pub enum SourceFormat {
    Json,
    Yaml,
}

enum Ordered {
    Scalar(Value),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}
impl Ordered {
    fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn into_value(self) -> Value {
        match self {
            Self::Scalar(v) => v,
            Self::Array(v) => Value::Array(v.into_iter().map(Self::into_value).collect()),
            Self::Object(v) => {
                Value::Object(v.into_iter().map(|(k, v)| (k, v.into_value())).collect())
            }
        }
    }
}
impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut remaining = Budget {
            nodes: MAX_NODES,
            string_bytes: MAX_EXPANDED_STRING_BYTES,
        };
        Seed {
            depth: 0,
            remaining: &mut remaining,
        }
        .deserialize(deserializer)
    }
}
struct Seed<'a> {
    depth: usize,
    remaining: &'a mut Budget,
}
struct Budget {
    nodes: usize,
    string_bytes: usize,
}
fn consume_string<E: de::Error>(budget: &mut Budget, len: usize) -> Result<(), E> {
    budget.string_bytes = budget
        .string_bytes
        .checked_sub(len)
        .ok_or_else(|| E::custom("expanded document string bound"))?;
    Ok(())
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Ordered;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Ordered, D::Error> {
        if self.depth > MAX_DEPTH || self.remaining.nodes == 0 {
            return Err(de::Error::custom("source parsing bound"));
        }
        self.remaining.nodes -= 1;
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Ordered;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded local document")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Ordered, E> {
        Ok(Ordered::Scalar(Value::Null))
    }
    fn visit_none<E: de::Error>(self) -> Result<Ordered, E> {
        self.visit_unit()
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Ordered, E> {
        Ok(Ordered::Scalar(Value::Bool(v)))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Ordered, E> {
        Ok(Ordered::Scalar(Value::Number(v.into())))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Ordered, E> {
        Ok(Ordered::Scalar(Value::Number(v.into())))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Ordered, E> {
        Number::from_f64(v)
            .map(|v| Ordered::Scalar(Value::Number(v)))
            .ok_or_else(|| E::custom("non-finite document number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Ordered, E> {
        consume_string::<E>(self.remaining, v.len())?;
        Ok(Ordered::Scalar(Value::String(v.into())))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Ordered, E> {
        consume_string::<E>(self.remaining, v.len())?;
        Ok(Ordered::Scalar(Value::String(v)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            depth: self.depth + 1,
            remaining: self.remaining,
        })? {
            values.push(value);
        }
        Ok(Ordered::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
        let mut values = Vec::new();
        let mut names = BTreeSet::new();
        while let Some(name) = map.next_key::<String>()? {
            consume_string::<A::Error>(self.remaining, name.len())?;
            if !names.insert(name.clone()) {
                return Err(de::Error::custom("duplicate document key"));
            }
            let value = map.next_value_seed(Seed {
                depth: self.depth + 1,
                remaining: self.remaining,
            })?;
            values.push((name, value));
        }
        Ok(Ordered::Object(values))
    }
}

pub(super) fn parse(
    source: &[u8],
    format: SourceFormat,
) -> Result<(Value, Vec<String>), &'static str> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("specification: source byte limit exceeded");
    }
    let ordered: Ordered = match format {
        SourceFormat::Json => serde_json::from_slice(source)
            .map_err(|_| "specification: invalid or over-bound JSON source")?,
        SourceFormat::Yaml => serde_yaml::from_slice(source)
            .map_err(|_| "specification: invalid or over-bound YAML source")?,
    };
    // Capture names before conversion; never recover order from Value maps.
    let names = match ordered
        .get("components")
        .and_then(|v| v.get("securitySchemes"))
    {
        None => Vec::new(),
        Some(Ordered::Object(entries)) => entries.iter().map(|(name, _)| name.clone()).collect(),
        _ => return Err("specification: security schemes object required"),
    };
    let value = ordered.into_value();
    let mut oauth2 = Vec::new();
    for name in names {
        let scheme = &value["components"]["securitySchemes"][&name];
        let mut scheme = scheme;
        let mut seen = BTreeSet::new();
        while let Some(reference) = scheme.get("$ref") {
            let pointer = reference
                .as_str()
                .and_then(|r| r.strip_prefix('#'))
                .filter(|p| p.starts_with('/'))
                .ok_or("specification: only local security scheme references permitted")?;
            if !seen.insert(pointer) || seen.len() > MAX_DEPTH {
                return Err("specification: cyclic or over-bound security scheme reference");
            }
            scheme = value
                .pointer(pointer)
                .ok_or("specification: missing security scheme reference")?;
        }
        let kind = scheme
            .get("type")
            .and_then(Value::as_str)
            .ok_or("specification: security scheme type required")?;
        if kind == "oauth2" {
            oauth2.push(name);
        }
    }
    Ok((value, oauth2))
}

/// A JSON encoding of the same admitted metadata tree can be larger than its
/// raw YAML capture (aliases and escaping). This derived ceiling covers six
/// encoded bytes per admitted string byte plus sixteen per admitted node;
/// it does not widen the original string/node/depth admission budgets.
pub(super) fn parse_projection(source: &[u8]) -> Result<(Value, Vec<String>), &'static str> {
    const MAX_PROJECTION_BYTES: usize = 6 * MAX_EXPANDED_STRING_BYTES + 16 * MAX_NODES;
    if source.len() > MAX_PROJECTION_BYTES {
        return Err("specification: metadata encoding exceeds source tree budgets");
    }
    let ordered: Ordered = serde_json::from_slice(source)
        .map_err(|_| "specification: invalid or over-bound JSON metadata")?;
    // Capture names before conversion; never recover order from Value maps.
    let names = match ordered
        .get("components")
        .and_then(|v| v.get("securitySchemes"))
    {
        None => Vec::new(),
        Some(Ordered::Object(entries)) => entries.iter().map(|(name, _)| name.clone()).collect(),
        _ => return Err("specification: security schemes object required"),
    };
    let value = ordered.into_value();
    let mut oauth2 = Vec::new();
    for name in names {
        let scheme = &value["components"]["securitySchemes"][&name];
        let mut scheme = scheme;
        let mut seen = BTreeSet::new();
        while let Some(reference) = scheme.get("$ref") {
            let pointer = reference
                .as_str()
                .and_then(|r| r.strip_prefix('#'))
                .filter(|p| p.starts_with('/'))
                .ok_or("specification: only local security scheme references permitted")?;
            if !seen.insert(pointer) || seen.len() > MAX_DEPTH {
                return Err("specification: cyclic or over-bound security scheme reference");
            }
            scheme = value
                .pointer(pointer)
                .ok_or("specification: missing security scheme reference")?;
        }
        let kind = scheme
            .get("type")
            .and_then(Value::as_str)
            .ok_or("specification: security scheme type required")?;
        if kind == "oauth2" {
            oauth2.push(name);
        }
    }
    Ok((value, oauth2))
}
