//! E-08 assertion inputs come from fixture signing recipes, never output claims.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(crate) fn needed(f: &Value) -> bool {
    crate::application::cases(f).iter().any(|case| {
        ["expect", "expectJava"].iter().any(|side| {
            case[*side]["backendCalls"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c.get("eventAuthorizer").is_some())
        })
    })
}

pub(crate) fn epochs(result: &Value, step: usize) -> Result<&Value> {
    let records = result["tokenSigningEpochs"]
        .as_array()
        .context("signing-time evidence required")?;
    let records = records
        .iter()
        .filter(|r| r["step"].as_u64() == Some(step as u64))
        .collect::<Vec<_>>();
    ensure!(
        records.len() == 1,
        "one signing-time record required per step"
    );
    Ok(&records[0]["epochs"])
}

pub(crate) fn bind(expected: &Value, fixture: &Value, epochs: Option<&Value>) -> Result<Value> {
    let mut expected = expected.clone();
    for call in expected["backendCalls"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        let Some(recipe) = call.get("eventAuthorizer") else {
            continue;
        };
        let reference = recipe["claimsFromToken"]
            .as_str()
            .context("signed token recipe reference")?;
        let name = reference
            .strip_prefix("${token:")
            .and_then(|s| s.strip_suffix('}'))
            .context("authorizer requires fixture signing recipe")?;
        let token = fixture["tokens"]
            .get(name)
            .context("unknown authorizer token")?;
        let epoch = epochs
            .context("authorizer signing evidence missing")?
            .get(name)
            .and_then(Value::as_i64)
            .context("authorizer signing epoch missing")?;
        let mut claims = token["claims"].clone();
        ensure!(
            claims.is_object()
                && ["iat", "exp", "nbf"]
                    .iter()
                    .all(|k| claims.get(k).is_none()),
            "authorizer time claims must come from signing recipe"
        );
        claims["iat"] = json!(epoch + token["issuedOffsetSec"].as_i64().context("issued offset")?);
        claims["exp"] = json!(epoch + token["expOffsetSec"].as_i64().context("expiry offset")?);
        if let Some(offset) = token["nbfOffsetSec"].as_i64().filter(|n| *n != 0) {
            claims["nbf"] = json!(epoch + offset);
        }
        let mut exact = json!({"principalId":recipe["principalId"],"claims":claims});
        if let Some(scope) = recipe.get("scope") {
            exact["scope"] = scope.clone();
        }
        call["eventAuthorizer"] = exact;
    }
    Ok(expected)
}

pub(crate) fn bind_result(
    expected: &Value,
    fixture: &Value,
    result: &Value,
    step: usize,
) -> Result<Value> {
    let needs = expected["backendCalls"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c.get("eventAuthorizer").is_some());
    bind(
        expected,
        fixture,
        if needs {
            Some(epochs(result, step)?)
        } else {
            None
        },
    )
}
