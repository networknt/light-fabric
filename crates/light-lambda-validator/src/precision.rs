use serde_json::Value;
use std::io;

use num_bigint::BigInt;
use std::{cmp::Ordering, str::FromStr};
// Compact exact base-10 value. Scale size follows the exponent lexeme, never
// the exponent's mathematical magnitude. No repeated-zero expansion.
#[derive(Debug)]
struct Decimal {
    negative: bool,
    digits: String,
    scale: BigInt,
}
impl Decimal {
    fn parse(input: &str) -> Result<Self, ()> {
        let (mantissa, exponent) = input.split_once(['e', 'E']).unwrap_or((input, "0"));
        let scale = BigInt::from_str(exponent).map_err(|_| ())?;
        let negative = mantissa.starts_with('-');
        let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
        let mut parts = mantissa.split('.');
        let whole = parts.next().ok_or(())?;
        let fraction = parts.next().unwrap_or("");
        if parts.next().is_some()
            || whole.is_empty()
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return Err(());
        }
        let mut digits = String::with_capacity(whole.len() + fraction.len());
        digits.push_str(whole);
        digits.push_str(fraction);
        let leading = digits.bytes().take_while(|b| *b == b'0').count();
        if leading == digits.len() {
            return Ok(Self {
                negative: false,
                digits: "0".into(),
                scale: BigInt::from(0),
            });
        }
        digits.drain(..leading);
        let trailing = digits.bytes().rev().take_while(|b| *b == b'0').count();
        digits.truncate(digits.len() - trailing);
        Ok(Self {
            negative,
            digits,
            scale: scale - BigInt::from(fraction.len()) + BigInt::from(trailing),
        })
    }
    fn compare(&self, rhs: &Self) -> Ordering {
        let a_zero = self.digits == "0";
        let b_zero = rhs.digits == "0";
        if a_zero && b_zero {
            return Ordering::Equal;
        }
        if self.negative != rhs.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let magnitude = if a_zero {
            Ordering::Less
        } else if b_zero {
            Ordering::Greater
        } else {
            let left = &self.scale + BigInt::from(self.digits.len());
            let right = &rhs.scale + BigInt::from(rhs.digits.len());
            left.cmp(&right).then_with(|| {
                let n = self.digits.len().max(rhs.digits.len());
                self.digits
                    .bytes()
                    .chain(std::iter::repeat(b'0'))
                    .take(n)
                    .cmp(rhs.digits.bytes().chain(std::iter::repeat(b'0')).take(n))
            })
        };
        if self.negative {
            magnitude.reverse()
        } else {
            magnitude
        }
    }
}
fn exact(input: &str) -> Result<Decimal, ()> {
    Decimal::parse(input)
}
struct ExactBound {
    kind: &'static str,
    limit: Decimal,
    lexeme: String,
}
impl jsonschema::Keyword for ExactBound {
    fn is_valid(&self, value: &Value) -> bool {
        let Some(value) = value.as_number() else {
            return true;
        };
        let Ok(value) = exact(value.as_str()) else {
            return false;
        };
        let comparison = value.compare(&self.limit);
        match self.kind {
            "maximum" => comparison != Ordering::Greater,
            "minimum" => comparison != Ordering::Less,
            "exclusiveMaximum" => comparison == Ordering::Less,
            _ => comparison == Ordering::Greater,
        }
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if let Some(value) = value.as_number()
            && exact(value.as_str()).is_err()
        {
            return Err(jsonschema::ValidationError::custom(
                "UNQUALIFIED_NUMERIC_REPRESENTATION",
            ));
        }
        if self.is_valid(value) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom(format!(
                "must have {} value of {}",
                match self.kind {
                    "maximum" => "a maximum",
                    "minimum" => "a minimum",
                    "exclusiveMaximum" => "an exclusive maximum",
                    _ => "an exclusive minimum",
                },
                self.lexeme
            )))
        }
    }
}

// No expanded-digit admission cap: exact bound work follows captured lexemes.
fn within_numeric_profile(value: &Value) -> bool {
    match value {
        Value::Number(n) => exact(n.as_str()).is_ok(),
        Value::Array(a) => a.iter().all(within_numeric_profile),
        Value::Object(o) => o.values().all(within_numeric_profile),
        _ => true,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            exact(a.as_str())
                .unwrap()
                .compare(&exact(b.as_str()).unwrap())
                == Ordering::Equal
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, a)| b.get(k).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}
struct ExactType(Vec<String>);
impl jsonschema::Keyword for ExactType {
    fn is_valid(&self, value: &Value) -> bool {
        self.0.iter().any(|t| match t.as_str() {
            "integer" => value.as_number().is_some_and(|n| {
                let n = exact(n.as_str()).unwrap();
                n.digits == "0" || n.scale >= BigInt::from(0)
            }),
            "number" => value.is_number(),
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            "string" => value.is_string(),
            _ => false,
        })
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if self.is_valid(value) {
            Ok(())
        } else {
            let expected = if self.0.len() == 1 {
                self.0[0].clone()
            } else {
                format!("[{}]", self.0.join(", "))
            };
            Err(jsonschema::ValidationError::custom(format!(
                "{} found, {expected} expected",
                kind(value)
            )))
        }
    }
}
struct ExactMultiple {
    limit: Decimal,
    lexeme: String,
}
impl ExactMultiple {
    fn divides(&self, number: &Decimal) -> bool {
        use num_bigint::BigUint;
        if number.digits == "0" {
            return true;
        }
        let shift = &number.scale - &self.limit.scale;
        if shift < BigInt::from(0) {
            return false;
        }
        let a = BigUint::from_str(&number.digits).unwrap();
        let b = BigUint::from_str(&self.limit.digits).unwrap();
        let (mut x, mut y) = (a, b.clone());
        let zero = BigUint::from(0u8);
        while y != zero {
            let next = &x % &y;
            x = y;
            y = next
        }
        let mut denominator = b / x;
        let twos = denominator.trailing_zeros().unwrap_or(0);
        denominator >>= twos as usize;
        let five = BigUint::from(5u8);
        let chunk = five.pow(27);
        let mut fives = 0u64;
        while &denominator % &chunk == zero {
            denominator /= &chunk;
            fives += 27
        }
        while &denominator % &five == zero {
            denominator /= &five;
            fives += 1
        }
        denominator == BigUint::from(1u8) && shift >= BigInt::from(twos.max(fives))
    }
}
impl jsonschema::Keyword for ExactMultiple {
    fn is_valid(&self, value: &Value) -> bool {
        value
            .as_number()
            .is_none_or(|n| self.divides(&exact(n.as_str()).unwrap()))
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if self.is_valid(value) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom(format!(
                "must be multiple of {}",
                self.lexeme
            )))
        }
    }
}
struct ExactValues {
    values: Vec<Value>,
    constant: bool,
}
impl jsonschema::Keyword for ExactValues {
    fn is_valid(&self, value: &Value) -> bool {
        self.values.iter().any(|v| equal(v, value))
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if self.is_valid(value) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom(if self.constant {
                format!(
                    "must be the constant value '{}'",
                    self.values[0]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| self.values[0].to_string())
                )
            } else {
                format!(
                    "does not have a value in the enumeration [{}]",
                    self.values
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }))
        }
    }
}
struct ExactUnique;
impl jsonschema::Keyword for ExactUnique {
    fn is_valid(&self, value: &Value) -> bool {
        value.as_array().is_none_or(|a| {
            a.iter()
                .enumerate()
                .all(|(i, v)| a[..i].iter().all(|other| !equal(v, other)))
        })
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if self.is_valid(value) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom(
                "must have only unique items in the array",
            ))
        }
    }
}

struct ExactCount {
    kind: &'static str,
    limit: Decimal,
    lexeme: String,
}
impl ExactCount {
    fn count(&self, value: &Value) -> Option<usize> {
        match self.kind {
            "minLength" | "maxLength" => value.as_str().map(|s| s.chars().count()),
            "minItems" | "maxItems" => value.as_array().map(Vec::len),
            _ => value.as_object().map(serde_json::Map::len),
        }
    }
}
impl jsonschema::Keyword for ExactCount {
    fn is_valid(&self, value: &Value) -> bool {
        self.count(value).is_none_or(|n| {
            let comparison = exact(&n.to_string()).unwrap().compare(&self.limit);
            if self.kind.starts_with("min") {
                comparison != Ordering::Less
            } else {
                comparison != Ordering::Greater
            }
        })
    }
    fn validate<'a>(&self, value: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        if self.is_valid(value) {
            return Ok(());
        }
        let n = self.count(value).unwrap();
        let detail = match self.kind {
            "minLength" => format!("must be at least {} characters long", self.lexeme),
            "maxLength" => format!("must be at most {} characters long", self.lexeme),
            "minItems" => format!("must have at least {} items but found {n}", self.lexeme),
            "maxItems" => format!("must have at most {} items but found {n}", self.lexeme),
            "minProperties" => format!("must have at least {} properties", self.lexeme),
            _ => format!("must have at most {} properties", self.lexeme),
        };
        Err(jsonschema::ValidationError::custom(detail))
    }
}
struct Always;
impl jsonschema::Keyword for Always {
    fn is_valid(&self, _: &Value) -> bool {
        true
    }
    fn validate<'a>(&self, _: &'a Value) -> Result<(), jsonschema::ValidationError<'a>> {
        Ok(())
    }
}
struct Deny;
impl jsonschema::Retrieve for Deny {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("uncatalogued".into())
    }
}
fn validation_active(context: &jsonschema::KeywordCompilationContext<'_>) -> bool {
    matches!(
        context.draft(),
        jsonschema::Draft::Draft4 | jsonschema::Draft::Draft6 | jsonschema::Draft::Draft7
    ) || context.has_vocabulary(&referencing::Vocabulary::Validation)
}
fn options() -> jsonschema::ValidationOptions<'static> {
    let mut options = jsonschema::options()
        .with_retriever(Deny)
        .should_validate_formats(false);
    for kind in ["maximum", "minimum", "exclusiveMaximum", "exclusiveMinimum"] {
        options = options.with_contextual_keyword(kind, move |context, parent, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            let kind = if context.draft() == jsonschema::Draft::Draft4 {
                match kind {
                    "exclusiveMaximum" | "exclusiveMinimum" => return Ok(None),
                    "maximum" if parent.get("exclusiveMaximum") == Some(&Value::Bool(true)) => {
                        "exclusiveMaximum"
                    }
                    "minimum" if parent.get("exclusiveMinimum") == Some(&Value::Bool(true)) => {
                        "exclusiveMinimum"
                    }
                    _ => kind,
                }
            } else {
                kind
            };
            let limit = value
                .as_number()
                .and_then(|n| exact(n.as_str()).ok())
                .ok_or_else(|| jsonschema::ValidationError::schema("invalid exact bound"))?;
            Ok(Some(Box::new(ExactBound {
                kind,
                limit,
                lexeme: value.to_string(),
            })))
        });
    }
    for kind in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        options = options.with_contextual_keyword(kind, move |context, _, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            let limit = value
                .as_number()
                .and_then(|n| exact(n.as_str()).ok())
                .filter(|n| !n.negative && (n.digits == "0" || n.scale >= BigInt::from(0)))
                .ok_or_else(|| jsonschema::ValidationError::schema("invalid exact count"))?;
            Ok(Some(Box::new(ExactCount {
                kind,
                limit,
                lexeme: value.to_string(),
            })))
        });
    }
    options = options
        .with_contextual_keyword("type", |context, _, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            let allowed = if let Some(s) = value.as_str() {
                vec![s.to_owned()]
            } else {
                value
                    .as_array()
                    .ok_or_else(|| jsonschema::ValidationError::schema("invalid type"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| jsonschema::ValidationError::schema("invalid type"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            if allowed.is_empty()
                || allowed
                    .iter()
                    .enumerate()
                    .any(|(i, s)| allowed[..i].contains(s))
                || allowed.iter().any(|s| {
                    ![
                        "integer", "number", "string", "null", "boolean", "array", "object",
                    ]
                    .contains(&s.as_str())
                })
            {
                return Err(jsonschema::ValidationError::schema("invalid type"));
            }
            Ok(Some(Box::new(ExactType(allowed))))
        })
        .with_contextual_keyword("multipleOf", |context, _, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            let limit = value
                .as_number()
                .and_then(|n| exact(n.as_str()).ok())
                .filter(|n| !n.negative && n.digits != "0")
                .ok_or_else(|| jsonschema::ValidationError::schema("invalid divisor"))?;
            Ok(Some(Box::new(ExactMultiple {
                limit,
                lexeme: value.to_string(),
            })))
        })
        .with_contextual_keyword("const", |context, _, value, _| {
            if !validation_active(context) || matches!(context.draft(), jsonschema::Draft::Draft4) {
                return Ok(None);
            }
            Ok(Some(Box::new(ExactValues {
                values: vec![value.clone()],
                constant: true,
            })))
        })
        .with_contextual_keyword("enum", |context, _, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            let values = value
                .as_array()
                .ok_or_else(|| jsonschema::ValidationError::schema("invalid enum"))?
                .clone();
            Ok(Some(Box::new(ExactValues {
                values,
                constant: false,
            })))
        })
        .with_contextual_keyword("uniqueItems", |context, _, value, _| {
            if !validation_active(context) {
                return Ok(None);
            }
            match value.as_bool() {
                Some(true) => Ok(Some(Box::new(ExactUnique) as Box<dyn jsonschema::Keyword>)),
                Some(false) => Ok(Some(Box::new(Always) as Box<dyn jsonschema::Keyword>)),
                None => Err(jsonschema::ValidationError::schema("invalid uniqueItems")),
            }
        });
    options
}
pub fn compile_map(schema: &Value, document: &Value) -> io::Result<jsonschema::ValidatorMap> {
    let mut registry = referencing::SPECIFICATIONS
        .add("urn:wp12:captured", document)
        .map_err(io::Error::other)?
        .add("urn:wp12:index", schema)
        .map_err(io::Error::other)?;
    for pointer in crate::catalogue::locations(document).map_err(io::Error::other)? {
        registry = registry
            .add_schema_entry("urn:wp12:captured", &pointer)
            .map_err(io::Error::other)?;
    }
    let registry = registry
        .retriever(Deny)
        .prepare()
        .map_err(io::Error::other)?;
    registry
        .validate_schema_entry_contexts()
        .map_err(io::Error::other)?;
    // Eager metaschema validation uses the same exact arithmetic/type/enum
    // adapters. Native cached metaschema validators lose extreme integer types.
    // All values remain original; no numeric projection participates here.
    {
        let mut metas = std::collections::BTreeMap::<String, jsonschema::Validator>::new();
        let boundaries = registry.schema_entry_boundaries();
        let default_meta = document
            .get("jsonSchemaDialect")
            .and_then(Value::as_str)
            .unwrap_or("https://json-schema.org/draft/2020-12/schema");
        // Prepare every actual captured metaschema before any validation publishes
        // readiness. No source reopen or request-time compilation.
        for entry in boundaries.entries() {
            let uri = entry
                .metaschema_uri()
                .map_err(io::Error::other)?
                .map_or_else(|| default_meta.to_owned(), |uri| uri.as_str().to_owned());
            if let std::collections::btree_map::Entry::Vacant(entry) = metas.entry(uri) {
                let meta = options()
                    .with_registry(&registry)
                    .build(&serde_json::json!({"$ref":entry.key()}))
                    .map_err(io::Error::other)?;
                entry.insert(meta);
            }
        }
        let prepared =
            jsonschema::Validator::prepare_schema_entries(&boundaries, &metas, default_meta)
                .map_err(io::Error::other)?;
        for pointer in crate::catalogue::locations(document).map_err(io::Error::other)? {
            let captured = document
                .pointer(&pointer)
                .ok_or_else(|| io::Error::other("missing captured schema"))?;
            let entry = boundaries
                .entry_for(captured)
                .ok_or_else(|| io::Error::other("missing captured schema context"))?;
            let uri = entry
                .metaschema_uri()
                .map_err(io::Error::other)?
                .map_or_else(|| default_meta.to_owned(), |uri| uri.as_str().to_owned());
            metas[&uri]
                .validate_prepared_schema_entry(captured, &prepared)
                .map_err(|error| io::Error::other(error.to_owned()))?;
        }
        // Foreign resources are never skipped: validate each against its own actual
        // prepared metaschema, including unselected/otherwise unreferenced resources.
        for entry in boundaries
            .entries()
            .filter(|entry| entry.is_resource_root())
        {
            let uri = entry
                .metaschema_uri()
                .map_err(io::Error::other)?
                .map_or_else(|| default_meta.to_owned(), |uri| uri.as_str().to_owned());
            metas[&uri]
                .validate_prepared_schema_entry(entry.contents(), &prepared)
                .map_err(|error| io::Error::other(error.to_owned()))?;
        }
    } // Release eager metaschema validators/targets before request-schema compilation.
    let map = options()
        .with_registry(&registry)
        .build_map(schema)
        .map_err(io::Error::other)?;
    if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
        for (key, value) in defs {
            if !map.contains_key(&format!("#/$defs/{key}")) {
                options()
                    .with_registry(&registry)
                    .build(value)
                    .map_err(io::Error::other)?;
            }
        }
    }
    Ok(map)
}

pub fn numeric_admitted(value: &Value) -> bool {
    within_numeric_profile(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_zero_scale_and_arbitrary_exponents() {
        for (a, b, expected) in [
            ("1e400", "1e401", Ordering::Less),
            ("-1e400", "-1e401", Ordering::Greater),
            ("-0e999999999999999999999999", "0", Ordering::Equal),
            (
                "1.00000000000000000001",
                "1.00000000000000000000",
                Ordering::Greater,
            ),
            ("10e-1", "1.0", Ordering::Equal),
            (
                "1e10000000000000000000000000000",
                "9e9999999999999999999999999999",
                Ordering::Greater,
            ),
            (
                "1e-10000000000000000000000000000",
                "9e-9999999999999999999999999999",
                Ordering::Less,
            ),
            ("1001e-3", "1", Ordering::Greater),
        ] {
            assert_eq!(
                exact(a).unwrap().compare(&exact(b).unwrap()),
                expected,
                "{a} / {b}"
            );
        }
    }
    #[test]
    fn all_small_pairs_match_rational_oracle() {
        use fraction::BigFraction;
        let values: Vec<_> = (-40..=40)
            .flat_map(|n| (0..=3).map(move |scale| format!("{n}.{}", "012".get(..scale).unwrap())))
            .filter(|s| !s.ends_with('.'))
            .collect();
        for a in &values {
            for b in &values {
                let rational = BigFraction::from_str(a)
                    .unwrap()
                    .partial_cmp(&BigFraction::from_str(b).unwrap())
                    .unwrap();
                assert_eq!(
                    exact(a).unwrap().compare(&exact(b).unwrap()),
                    rational,
                    "{a}/{b}"
                );
            }
        }
    }
    #[test]
    fn magnitude_does_not_determine_coefficient_allocation() {
        let n = exact("1e999999999999999999999999999999999999999999999999999999").unwrap();
        assert_eq!(n.digits.len(), 1);
        assert!(format!("{}", n.scale).len() < 60);
    }
}

#[cfg(test)]
mod exact_keyword_tests {
    use super::*;
    #[test]
    fn exact_divisibility_against_small_rational_grid() {
        for a in -30..=30 {
            for b in 1..=30 {
                for shift in -2i32..=2 {
                    let divisor = ExactMultiple {
                        limit: exact(&format!("{b}e{shift}")).unwrap(),
                        lexeme: String::new(),
                    };
                    let n = exact(&format!("{a}.25")).unwrap();
                    let scale = 10i64.pow(shift.unsigned_abs());
                    let numerator = 4 * a + if a < 0 { -1 } else { 1 };
                    let expected = if shift >= 0 {
                        numerator % (4 * b * scale) == 0
                    } else {
                        (numerator * scale) % (4 * b) == 0
                    };
                    assert_eq!(divisor.divides(&n), expected, "{a}.25/{b}e{shift}");
                }
            }
        }
    }
    #[test]
    fn huge_exponents_need_no_expanded_integer() {
        let even = ExactMultiple {
            limit: exact("2").unwrap(),
            lexeme: String::new(),
        };
        let three = ExactMultiple {
            limit: exact("3").unwrap(),
            lexeme: String::new(),
        };
        let n = exact("1e99999999999999999999999999999999999").unwrap();
        assert!(even.divides(&n));
        assert!(!three.divides(&n));
    }
}
