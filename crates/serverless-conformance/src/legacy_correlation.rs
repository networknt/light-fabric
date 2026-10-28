//! Two archived native scalar-correlation descriptors; no arbitrary field mask.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
pub(crate) fn supported(f: &Value) -> bool {
    [
        "B-05-null-response-headers-cp1-e02-accepted",
        "Q-02-generated-null-map-cp1-e02-accepted",
    ]
    .contains(&f["id"].as_str().unwrap_or(""))
        && f["nondeterministic"]
            == json!([{"field":"/backendCalls/0/event/headers/X-Correlation-Id","header":"x-correlation-id","pattern":"^[A-Za-z0-9_-]{22}$"}])
}
pub(crate) fn bind(f: &Value, java: &Value) -> Result<Value> {
    let mut f = f.clone();
    if f["nondeterministic"].as_array().is_some_and(Vec::is_empty) {
        return Ok(f);
    }
    ensure!(supported(&f), "unsupported native correlation descriptor");
    let observations = java["observations"]
        .as_array()
        .context("Java correlation observations")?;
    ensure!(
        observations.len() == 1,
        "closed scalar correlation case count"
    );
    let actual = observations[0]
        .pointer("/backendCalls/0/event/headers/X-Correlation-Id")
        .and_then(Value::as_str)
        .context("actual native scalar correlation")?;
    ensure!(
        actual.len() == 22
            && actual
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "native scalar correlation violates original descriptor"
    );
    let expected = f["expectJava"]
        .pointer_mut("/backendCalls/0/event/headers/X-Correlation-Id")
        .context("original scalar correlation expectation")?;
    ensure!(expected.is_string(), "scalar correlation expected type");
    *expected = json!(actual);
    f["nondeterministic"] = json!([]);
    Ok(f)
}
