//! Closed native expectation recipes. No arbitrary field masks or policy logic.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

pub(crate) fn needs_clock(case: &Value) -> bool {
    ["expect", "expectJava"].iter().any(|side| {
        case[*side]["backendCalls"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c.get("eventClock").is_some())
    })
}

pub(crate) fn window(result: &Value, step: usize) -> Result<Option<&Value>> {
    let matches = result["invocationWindows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["step"].as_u64() == Some(step as u64))
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "duplicate invocation wall-clock windows"
    );
    Ok(matches.first().copied())
}

pub(crate) fn prepare(e: &Value, a: &Value, window: Option<&Value>) -> Result<Value> {
    let mut e = e.clone();
    for (i, call) in e["backendCalls"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let actual = &a["backendCalls"][i];
        if let Some(expected) = call.get("eventAuthorizer").cloned() {
            ensure!(
                expected.get("claims").is_some() && expected.get("claimsFromToken").is_none(),
                "authorizer assertion requires bound signed input"
            );
            ensure!(
                actual["event"]["requestContext"]["authorizer"] == expected,
                "backend authorizer differs from exact signed input claims/principal/scope"
            );
            call.as_object_mut().unwrap().remove("eventAuthorizer");
        }
        if let Some(profile) = call.get("eventClock") {
            ensure!(
                profile == "invocation-wall-clock",
                "unsupported backend clock recipe"
            );
            let w = window.context("actual invocation wall-clock window required")?;
            let start = w["startEpochMs"].as_u64().context("window start")?;
            let end = w["endEpochMs"].as_u64().context("window end")?;
            let c = &actual["event"]["requestContext"];
            let epoch = c["requestTimeEpoch"]
                .as_u64()
                .context("nonnegative integral request epoch")?;
            ensure!(
                start <= epoch && epoch <= end,
                "request time outside actual invocation window"
            );
            let text = c["requestTime"].as_str().context("request time text")?;
            let parsed = chrono::DateTime::parse_from_str(text, "%d/%b/%Y:%H:%M:%S %z")?;
            ensure!(
                parsed.offset().local_minus_utc() == 0
                    && parsed.timestamp() == (epoch / 1000) as i64
                    && parsed.format("%d/%b/%Y:%H:%M:%S %z").to_string() == text,
                "request time text/epoch/UTC format mismatch"
            );
            call.as_object_mut().unwrap().remove("eventClock");
        }
        if let Some(profile) = call.get("eventCorrelation") {
            let (name, rust) = match profile.as_str() {
                Some("java-base64url128-v1") => ("X-Correlation-Id", false),
                Some("rust-base64url128-v1") => ("x-correlation-id", true),
                _ => anyhow::bail!("unsupported generated correlation recipe"),
            };
            let value = actual["event"]["headers"][name]
                .as_str()
                .context("generated correlation header")?;
            ensure!(
                value.len() == 22 && URL_SAFE_NO_PAD.decode(value)?.len() == 16,
                "generated correlation must be base64url128"
            );
            for container in ["event", "eventExact"] {
                ensure!(
                    call[container]["headers"][name] == "${generated:correlation}",
                    "generated correlation must remain in exact header maps"
                );
                call[container]["headers"][name] = Value::String(value.into());
                if rust && let Some(headers) = call[container].get_mut("multiValueHeaders") {
                    ensure!(
                        headers[name] == serde_json::json!(["${generated:correlation}"]),
                        "generated correlation multi-value coupling required"
                    );
                    headers[name] = serde_json::json!([value]);
                }
            }
            call.as_object_mut().unwrap().remove("eventCorrelation");
        }
    }
    if let Some(calls) = e.get("httpCalls") {
        e["httpCalls"] = http_shape(calls);
    }
    // Standard HTTP subset assertions subsequently enforce these exact three
    // header value lists, including multiplicity, against actual wire headers.
    Ok(e)
}

pub(crate) fn http_shape(calls: &Value) -> Value {
    let mut calls = calls.clone();
    for call in calls.as_array_mut().into_iter().flatten() {
        if let Some(recipe) = call.get("oauthRequest").cloned() {
            for name in ["authorization", "content-type", "accept"] {
                call["headers"][name] = serde_json::json!([recipe[name]]);
            }
            call.as_object_mut().unwrap().remove("oauthRequest");
        }
    }
    calls
}

pub(crate) fn http_calls(expected: &Value, actual: &Value) -> Value {
    let mut actual = actual.clone();
    for (i, call) in actual.as_array_mut().into_iter().flatten().enumerate() {
        if expected[i].get("oauthRequest").is_some() {
            let mut headers = serde_json::Map::new();
            for (name, values) in call["headers"].as_object().into_iter().flatten() {
                let entry = headers
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| serde_json::json!([]));
                entry
                    .as_array_mut()
                    .unwrap()
                    .extend(values.as_array().into_iter().flatten().cloned());
            }
            call["headers"] = Value::Object(headers);
        }
    }
    actual
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn clock_recipe_rejects_missing_window_outside_epoch_and_uncoupled_text() {
        let e = json!({"backendCalls":[{"eventClock":"invocation-wall-clock"}]});
        let a = json!({"backendCalls":[{"event":{"requestContext":{"requestTimeEpoch":1000,"requestTime":"01/Jan/1970:00:00:01 +0000"}}}]});
        let w = json!({"startEpochMs":999,"endEpochMs":1001});
        assert!(prepare(&e, &a, Some(&w)).is_ok());
        assert!(prepare(&e, &a, None).is_err());
        assert!(
            prepare(
                &e,
                &a,
                Some(&json!({"startEpochMs":1001,"endEpochMs":2000}))
            )
            .is_err()
        );
        let mut bad = a.clone();
        bad["backendCalls"][0]["event"]["requestContext"]["requestTime"] =
            json!("01/Jan/1970:00:00:02 +0000");
        assert!(prepare(&e, &bad, Some(&w)).is_err());
        assert!(window(&json!({"invocationWindows":[{"step":0},{"step":0}]}), 0).is_err());
    }
}
