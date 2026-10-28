//! Explicit activation ownership for local admin terminals; no generic defaults.
use crate::init_error::InitError;
use serde_json::{Value, json};
use serverless_core::HandlerId;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn reject_logger_set_selection(
    handler: &Value,
    setter_enabled: bool,
) -> Result<(), InitError> {
    if setter_enabled {
        return Ok(());
    }
    let invalid = || InitError::InvalidConfiguration("handler.yml: invalid logger selection");
    fn structured(value: Option<&Value>) -> Result<Value, InitError> {
        match value {
            None | Some(Value::Null) => Ok(Value::Null),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(Value::Null),
            Some(Value::String(s)) => serde_json::from_str(s).map_err(|_| {
                InitError::InvalidConfiguration("handler.yml: invalid structured value")
            }),
            Some(v) => Ok(v.clone()),
        }
    }
    fn strings(value: Option<&Value>) -> Result<Vec<String>, InitError> {
        if let Some(Value::String(s)) = value
            && !s.trim().starts_with('[')
        {
            return Ok(if s.trim().is_empty() {
                vec![]
            } else {
                s.split(',').map(|v| v.trim().into()).collect()
            });
        }
        let value = structured(value)?;
        if value.is_null() {
            return Ok(vec![]);
        }
        serde_json::from_value(value)
            .map_err(|_| InitError::InvalidConfiguration("handler.yml: invalid string list"))
    }
    let mut aliases = BTreeMap::new();
    for key in ["handlers", "additionalHandlers"] {
        for entry in strings(handler.get(key))? {
            let (class, alias) = entry.split_once('@').unwrap_or((&entry, &entry));
            let setter = serverless_core::compat::lookup(class)
                .is_some_and(|c| c.id == HandlerId::SetLogger);
            aliases.insert(alias.to_owned(), setter);
        }
    }
    let mut chains = BTreeMap::new();
    for key in ["chains", "additionalChains"] {
        let value = structured(handler.get(key))?;
        if !value.is_null() {
            for (name, exec) in value.as_object().ok_or_else(invalid)? {
                chains.insert(name.clone(), strings(Some(exec))?);
            }
        }
    }
    fn inspect(
        exec: &[String],
        aliases: &BTreeMap<String, bool>,
        chains: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), InitError> {
        for name in exec {
            if let Some(chain) = chains.get(name) {
                if !seen.insert(name.clone()) {
                    return Err(InitError::InvalidConfiguration("handler.yml: cyclic chain"));
                }
                inspect(chain, aliases, chains, seen)?;
                seen.remove(name);
            } else if aliases.get(name) == Some(&true) {
                return Err(InitError::InvalidConfiguration(
                    "logging.yml: logger SET unsupported",
                ));
            }
        }
        Ok(())
    }
    // Validate every declared executable chain, including currently unused ones.
    for exec in chains.values() {
        inspect(exec, &aliases, &chains, &mut BTreeSet::new())?;
    }
    inspect(
        &strings(handler.get("defaultHandlers"))?,
        &aliases,
        &chains,
        &mut BTreeSet::new(),
    )?;
    for key in ["paths", "additionalPaths"] {
        let value = structured(handler.get(key))?;
        if !value.is_null() {
            for path in value.as_array().ok_or_else(invalid)? {
                inspect(
                    &strings(path.get("exec"))?,
                    &aliases,
                    &chains,
                    &mut BTreeSet::new(),
                )?;
            }
        }
    }
    Ok(())
}

pub(crate) fn activation(
    id: HandlerId,
    value: &Value,
    setter_available: bool,
) -> Result<(bool, Vec<String>), InitError> {
    let invalid =
        || InitError::InvalidConfiguration("local admin: invalid or unsupported configuration");
    let object = value.as_object().ok_or_else(invalid)?;
    let (flag, prefix, defaults) = match id {
        HandlerId::Info => (
            "enableServerInfo",
            "info",
            vec![
                (
                    "keysToNotSort",
                    vec![
                        Value::Null,
                        json!(["admin", "default", "defaultHandlers", "request", "response"]),
                    ],
                ),
                ("downstreamEnabled", vec![json!(false)]),
                (
                    "downstreamHost",
                    vec![Value::Null, json!("http://localhost:8081")],
                ),
                (
                    "downstreamPath",
                    vec![Value::Null, json!("/adm/server/info")],
                ),
            ],
        ),
        HandlerId::GetLogger | HandlerId::SetLogger => (
            "enabled",
            "logger",
            vec![
                ("logStart", vec![json!(0), json!(600000)]),
                ("downstreamEnabled", vec![json!(false)]),
                (
                    "downstreamHost",
                    vec![Value::Null, json!("http://localhost:8081")],
                ),
                ("downstreamFramework", vec![Value::Null, json!("Light4j")]),
            ],
        ),
        _ => return Err(invalid()),
    };
    for key in object.keys() {
        if key != flag && !defaults.iter().any(|(name, _)| key == name) {
            return Err(invalid());
        }
    }
    // Java initialized fields are false when omitted from an existing map.
    // Template defaults belong to the template, not a universal activation fallback.
    let enabled = match object.get(flag) {
        None => false,
        Some(Value::Bool(enabled)) => *enabled,
        _ => return Err(invalid()),
    };
    let mut warnings = Vec::new();
    for (key, allowed) in defaults {
        let mode = match object.get(key) {
            None => "absent",
            Some(value) if allowed.contains(value) => "default",
            _ => return Err(invalid()),
        };
        warnings.push(format!("{prefix}.{key}: unsupported {mode} ignored"));
    }
    // A trusted opted-in factory is additionally required for partial X-04.
    Ok((
        enabled && (id != HandlerId::SetLogger || setter_available),
        warnings,
    ))
}
