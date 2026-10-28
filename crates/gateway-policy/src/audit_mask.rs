//! Bounded Java capture-group masking for approved safe audit selectors.
use crate::audit::{MAX_SELECTED_STRING_BYTES, MAX_SELECTORS, SAFE_FIELDS, safe_header};
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Default)]
pub struct AuditMask(BTreeMap<(String, String), Regex>);
impl AuditMask {
    pub fn from_regex(value: &Value) -> Result<Self, &'static str> {
        if value.is_null() || value.as_str() == Some("") {
            return Ok(Self::default());
        }
        let map = value.as_object().ok_or("mask.regex: object required")?;
        if map.len() > 2 {
            return Err("mask.regex: unqualified target");
        }
        let mut patterns = BTreeMap::new();
        for (group, fields) in map {
            if !["audit", "requestHeader"].contains(&group.as_str()) {
                return Err("mask.regex: unqualified target");
            }
            let fields = fields.as_object().ok_or("mask.regex: field map required")?;
            if fields.len() > MAX_SELECTORS {
                return Err("mask.regex: unbounded selectors");
            }
            for (name, pattern) in fields {
                if (group == "audit" && !SAFE_FIELDS.contains(&name.as_str()))
                    || (group == "requestHeader" && !safe_header(name))
                {
                    return Err("mask.regex: unsafe selector");
                }
                let pattern = pattern.as_str().ok_or("mask.regex: string required")?;
                if pattern.len() > 1024 {
                    return Err("mask.regex: pattern too large");
                }
                if pattern.is_empty() {
                    continue;
                }
                let regex = RegexBuilder::new(&format!("\\A(?:{pattern})\\z"))
                    .size_limit(65536)
                    .dfa_size_limit(65536)
                    .build()
                    .map_err(|_| "mask.regex: unsupported or invalid bounded pattern")?;
                patterns.insert((group.clone(), name.clone()), regex);
            }
        }
        Ok(Self(patterns))
    }
    pub fn apply(&self, group: &str, name: &str, value: &str) -> String {
        if value.len() > MAX_SELECTED_STRING_BYTES {
            return "<truncated>".into();
        }
        let Some(regex) = self.0.get(&(group.into(), name.into())) else {
            return value.into();
        };
        let Some(captures) = regex.captures(value) else {
            return value.into();
        };
        let mut output = value.to_owned();
        // Java matches the whole string, then replaces the first occurrence of
        // each original group value, using UTF16 length rather than byte length.
        for group in captures.iter().skip(1).flatten() {
            let replacement = "*".repeat(group.as_str().encode_utf16().count());
            output = output.replacen(group.as_str(), &replacement, 1);
        }
        output
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn real_mask_actions_follow_full_match_group_order_and_java_utf16_length() {
        let plan=AuditMask::from_regex(&json!({"requestHeader":{"x-test-header":"id=(.+);id=(.+)"},"audit":{"client_id":"(..)(.+)"}})).unwrap();
        assert_eq!(
            plan.apply("requestHeader", "x-test-header", "id=é😀;id=é😀"),
            "id=***;id=***"
        );
        assert_eq!(
            plan.apply("requestHeader", "x-test-header", "prefix id=a;id=b"),
            "prefix id=a;id=b"
        );
        assert_eq!(plan.apply("audit", "client_id", "abcd"), "****");
        assert_eq!(plan.apply("audit", "user_id", "unchanged"), "unchanged");
    }
    #[test]
    fn unsafe_targets_unbounded_and_non_linear_patterns_are_init_errors() {
        for invalid in [
            json!({"requestBody":{}}),
            json!({"requestHeader":{"authorization":"(.*)"}}),
            json!({"audit":{"requestBody":"(.*)"}}),
            json!({"audit":{"client_id":"(?=a)(a)"}}),
            json!({"audit":{"client_id":"x".repeat(1025)}}),
        ] {
            assert!(AuditMask::from_regex(&invalid).is_err());
        }
    }
}
