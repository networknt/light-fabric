//! Immutable program loading only. No regex substitute, worker launch, profile
//! admission or HTTP failure mapping is supplied by this module.
use crate::router_java_worker::Rule;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct LoadingLimits {
    pub rules: usize,
    pub per_rule_bytes: usize,
    pub aggregate_bytes: usize,
    pub token_utf16: usize,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Shape,
    Syntax,
    Profile,
    Allocation,
}
pub struct Program {
    rules: Arc<[Rule]>,
    identity: [u8; 32],
    source_bytes: usize,
}
impl Program {
    pub fn capture(value: &Value, limits: LoadingLimits) -> Result<Self, Failure> {
        // Reuse the existing native scalar/array/null configuration contract.
        let raw = serverless_core::unsupported::strings(value.get("urlRewriteRules"))
            .map_err(|_| Failure::Shape)?;
        if raw.len() > limits.rules {
            return Err(Failure::Profile);
        }
        let mut total = 0usize;
        // Admit the whole source before allocating any pattern/replacement.
        for rule in &raw {
            total = total.checked_add(rule.len()).ok_or(Failure::Profile)?;
            if rule.len() > limits.per_rule_bytes || total > limits.aggregate_bytes {
                return Err(Failure::Profile);
            }
            let mut parts = rule.split(' ').filter(|s| !s.is_empty());
            let from = parts.next().ok_or(Failure::Syntax)?;
            let to = parts.next().ok_or(Failure::Syntax)?;
            if parts.next().is_some() {
                return Err(Failure::Syntax);
            }
            if from.encode_utf16().count() > limits.token_utf16
                || to.encode_utf16().count() > limits.token_utf16
            {
                return Err(Failure::Profile);
            }
        }
        let mut rules = Vec::new();
        rules
            .try_reserve_exact(raw.len())
            .map_err(|_| Failure::Allocation)?;
        let mut identity = Sha256::new();
        identity.update(b"light-lambda/router-program/v1\0");
        for rule in raw {
            let mut parts = rule.split(' ').filter(|s| !s.is_empty());
            let pattern = parts.next().ok_or(Failure::Syntax)?;
            let replacement = parts.next().ok_or(Failure::Syntax)?;
            for token in [pattern, replacement] {
                identity.update((token.len() as u64).to_be_bytes());
                identity.update(token.as_bytes());
            }
            rules.push(Rule {
                pattern: pattern.to_owned(),
                replacement: replacement.to_owned(),
            });
        }
        Ok(Self {
            rules: rules.into(),
            identity: identity.finalize().into(),
            source_bytes: total,
        })
    }
    pub fn rules(&self) -> Arc<[Rule]> {
        self.rules.clone()
    }
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }
    /// Source payload size, not a complete memory bound.
    pub fn source_bytes(&self) -> usize {
        self.source_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn limits() -> LoadingLimits {
        LoadingLimits {
            rules: 2,
            per_rule_bytes: 64,
            aggregate_bytes: 128,
            token_utf16: 32,
        }
    }
    #[test]
    fn java_tokens_and_program_order_are_retained_without_regex_interpretation() {
        let mut source = json!({"urlRewriteRules": ["  ^/(x)$   /$1  ", "(?<=/)x /next"]});
        let program = Program::capture(&source, limits()).unwrap();
        source["urlRewriteRules"] = json!([]);
        let rules = program.rules();
        assert_eq!(rules[0].pattern, "^/(x)$");
        assert_eq!(rules[0].replacement, "/$1");
        assert_eq!(rules[1].pattern, "(?<=/)x");
        assert_ne!(
            program.identity(),
            Program::capture(&source, limits()).unwrap().identity()
        );
        assert_eq!(
            Program::capture(&json!({"urlRewriteRules":"^/(x)$ /$1"}), limits())
                .unwrap()
                .rules()[0]
                .replacement,
            "/$1"
        );
    }
    #[test]
    fn tabs_and_extra_tokens_are_not_silently_normalized() {
        for text in ["x\ty", "x y z", " "] {
            assert!(matches!(
                Program::capture(&json!({"urlRewriteRules":[text]}), limits()),
                Err(Failure::Syntax)
            ));
        }
        assert!(matches!(
            Program::capture(&json!({"urlRewriteRules":[1]}), limits()),
            Err(Failure::Shape)
        ));
    }
    #[test]
    fn exact_loading_boundaries_and_supplementary_utf16() {
        let source = json!({"urlRewriteRules":["😀 /x"]});
        let exact = LoadingLimits {
            rules: 1,
            per_rule_bytes: 7,
            aggregate_bytes: 7,
            token_utf16: 2,
        };
        assert_eq!(Program::capture(&source, exact).unwrap().source_bytes(), 7);
        for limits in [
            LoadingLimits {
                rules: 0,
                ..limits()
            },
            LoadingLimits {
                per_rule_bytes: 6,
                ..limits()
            },
            LoadingLimits {
                aggregate_bytes: 6,
                ..limits()
            },
            LoadingLimits {
                token_utf16: 1,
                ..limits()
            },
        ] {
            assert!(matches!(
                Program::capture(&source, limits),
                Err(Failure::Profile)
            ));
        }
    }
}
