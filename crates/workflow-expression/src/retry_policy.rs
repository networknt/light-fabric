//! The bounded v2 response-retry policy. Pure validation shared by admission and
//! execution of immutable snapshots; no evaluator or expression bindings.
use crate::{Category, Diagnostic, ExpressionError, Phase};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedRetry {
    /// Total attempts including the initial dispatch; omitted count defaults to 1.
    pub attempts: u16,
    /// Fixed interval; omitted delay defaults to zero.
    pub delay_ms: u64,
}
fn error(field: &str, task: Option<&str>, category: Category) -> Diagnostic {
    Diagnostic {
        error: ExpressionError {
            category,
            phase: Phase::Admission,
            span_index: None,
            offset: None,
        },
        task: task.map(str::to_owned),
        field: field.into(),
    }
}
fn path(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}
fn keys<'a>(
    value: &'a Value,
    field: &str,
    task: Option<&str>,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, Diagnostic> {
    let map = value
        .as_object()
        .ok_or_else(|| error(field, task, Category::Invalid))?;
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(error(&path(field, key), task, Category::Unsupported));
        }
    }
    Ok(map)
}

/// String references resolve only to an inline component in use.retries.
/// Nested `use`/alias/override forms are rejected rather than partly applied.
pub fn resolve_retry_policy(
    raw: &Value,
    value: &Value,
    field: &str,
    task: Option<&str>,
) -> Result<FixedRetry, Diagnostic> {
    let (value, field) = if let Some(name) = value.as_str() {
        let component = raw
            .pointer("/use/retries")
            .and_then(|v| v.get(name))
            .ok_or_else(|| error(field, task, Category::Unsupported))?;
        (component, path("/use/retries", name))
    } else {
        (value, field.to_owned())
    };
    let policy = keys(value, &field, task, &["limit", "delay"])?;
    let mut attempts = 1;
    if let Some(limit) = policy.get("limit") {
        let limit_path = path(&field, "limit");
        let limit = keys(limit, &limit_path, task, &["attempt"])?;
        if let Some(attempt) = limit.get("attempt") {
            let attempt_path = path(&limit_path, "attempt");
            let attempt = keys(attempt, &attempt_path, task, &["count"])?;
            if let Some(count) = attempt.get("count") {
                attempts = count
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| error(&path(&attempt_path, "count"), task, Category::Invalid))?;
            }
        }
    }
    let mut delay_ms: u64 = 0;
    if let Some(delay) = policy.get("delay") {
        let delay_path = path(&field, "delay");
        let delay = keys(
            delay,
            &delay_path,
            task,
            &["days", "hours", "minutes", "seconds", "milliseconds"],
        )?;
        for (unit, factor) in [
            ("days", 86_400_000),
            ("hours", 3_600_000),
            ("minutes", 60_000),
            ("seconds", 1_000),
            ("milliseconds", 1),
        ] {
            if let Some(value) = delay.get(unit) {
                delay_ms = value
                    .as_u64()
                    .and_then(|n| n.checked_mul(factor))
                    .and_then(|n| delay_ms.checked_add(n))
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or_else(|| error(&path(&delay_path, unit), task, Category::Invalid))?;
            }
        }
    }
    Ok(FixedRetry { attempts, delay_ms })
}

/// Inspect raw policies before typed serde conversion can discard unknown fields.
/// Call only for a known v2 profile, after profile/snapshot integrity checks.
pub fn validate_retry_policies(raw: &Value) -> Result<(), Diagnostic> {
    if let Some(policies) = raw.pointer("/use/retries") {
        let policies = policies
            .as_object()
            .ok_or_else(|| error("/use/retries", None, Category::Invalid))?;
        for (name, policy) in policies {
            // Component aliases are not in the typed component model.
            if !policy.is_object() {
                return Err(error(
                    &path("/use/retries", name),
                    None,
                    Category::Unsupported,
                ));
            }
            resolve_retry_policy(raw, policy, &path("/use/retries", name), None)?;
        }
    }
    fn tasks(raw: &Value, entries: &Value, field: &str) -> Result<(), Diagnostic> {
        let Some(entries) = entries.as_array() else {
            return Ok(());
        };
        for (i, entry) in entries.iter().enumerate() {
            if let Some(map) = entry.as_object() {
                for (name, task) in map {
                    let field = path(&path(field, &i.to_string()), name);
                    if let Some(retry) = task.get("retry") {
                        resolve_retry_policy(raw, retry, &path(&field, "retry"), Some(name))?;
                    }
                    if let Some(branches) = task.pointer("/fork/branches") {
                        tasks(raw, branches, &path(&path(&field, "fork"), "branches"))?;
                    }
                }
            }
        }
        Ok(())
    }
    if let Some(entries) = raw.get("do") {
        tasks(raw, entries, "/do")?;
    }
    Ok(())
}
