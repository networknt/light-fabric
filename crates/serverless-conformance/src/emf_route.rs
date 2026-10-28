//! Static admission of the existing closed EMF recipes, not runtime execution.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

pub(crate) fn validate(fixture: &Value, step: &Value) -> Result<()> {
    let path = step["rawEvent"]["path"]
        .as_str()
        .context("EMF request path required")?;
    let method = step["rawEvent"]["httpMethod"]
        .as_str()
        .context("EMF request method required")?;
    let expected = step["expect"]["emf"]["route"]
        .as_str()
        .context("EMF route required")?;
    let proxy = &fixture["config"]["files"]["lambda-proxy"];
    ensure!(
        proxy["enabled"].is_null() || proxy["enabled"] == true,
        "closed EMF proxy must be active"
    );
    let functions = proxy["functions"]
        .as_object()
        .context("EMF configured routes required")?;
    let mut shapes = BTreeSet::new();
    let mut best: Option<(&str, Vec<u8>)> = None;
    for (mapping, target) in functions {
        ensure!(
            target.is_string(),
            "EMF configured destination alias must be a string"
        );
        let (template, verb) = mapping
            .split_once('@')
            .context("EMF configured route method required")?;
        ensure!(
            !verb.is_empty() && verb.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-'),
            "EMF configured route method invalid"
        );
        ensure!(
            template.starts_with('/') && !template.contains(['?', '#', '@']),
            "EMF configured route invalid"
        );
        let segments: Vec<_> = template[1..].split('/').collect();
        let mut names = BTreeSet::new();
        let mut priority = Vec::new();
        let mut shape = Vec::new();
        for (i, segment) in segments.iter().enumerate() {
            if *segment == "*" && i + 1 == segments.len() {
                priority.push(1);
                shape.push("*");
            } else if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                ensure!(
                    !name.is_empty() && !name.contains(['{', '}', '*']) && names.insert(name),
                    "EMF configured parameter invalid"
                );
                priority.push(2);
                shape.push("{}");
            } else {
                ensure!(
                    !segment.contains(['{', '}', '*']),
                    "EMF configured segment invalid"
                );
                priority.push(3);
                shape.push(segment);
            }
        }
        ensure!(
            shapes.insert((verb.to_ascii_uppercase(), shape.join("/"))),
            "EMF configured route duplicate"
        );
        if !verb.eq_ignore_ascii_case(method) {
            continue;
        }
        let Some(path) = path.strip_prefix('/') else {
            continue;
        };
        let mut parts = path.split('/');
        let matches = segments.iter().enumerate().all(|(i, segment)| {
            let Some(part) = parts.next() else {
                return false;
            };
            if *segment == "*" && i + 1 == segments.len() {
                // Consume the remainder without creating/substituting captures.
                for _ in parts.by_ref() {}
                true
            } else if segment.starts_with('{') {
                !part.is_empty()
            } else {
                *segment == part
            }
        }) && parts.next().is_none();
        if matches
            && best
                .as_ref()
                .is_none_or(|(_, old)| priority.as_slice() > old.as_slice())
        {
            best = Some((template, priority));
        }
    }
    if let Some((template, _)) = best {
        ensure!(
            expected == template,
            "EMF route must equal its trusted configured template"
        );
    } else {
        // Only the owner-approved missing-target recipe extends the closed
        // profile. Other early exits keep their matching configured template.
        ensure!(
            fixture["id"] == "M-01-gateway-error-emf-multi-contract-v1"
                && path == "/pets/fixed"
                && method.eq_ignore_ascii_case("GET")
                && step["expect"]["status"] == 400
                && step["expect"]["backendCalls"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                && expected == "<unmatched>",
            "unmatched EMF route requires the approved missing-target recipe"
        );
    }
    Ok(())
}
