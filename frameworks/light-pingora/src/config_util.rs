use serde::Deserializer;
use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;

pub(crate) fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringListVisitor;

    impl<'de> Visitor<'de> for StringListVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string, JSON/YAML string list, or list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(parse_string_list(value))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(StringListVisitor)
}

pub(crate) fn deserialize_string_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringMapVisitor;

    impl<'de> Visitor<'de> for StringMapVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map, JSON/YAML string map, or key=value list")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            parse_string_map(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, serde_yaml::Value>()? {
                values.insert(key, yaml_scalar_to_string(value).map_err(A::Error::custom)?);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(StringMapVisitor)
}

pub(crate) fn deserialize_typed_map<'de, D, T>(
    deserializer: D,
) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct TypedMapVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T> Visitor<'de> for TypedMapVisitor<T>
    where
        T: serde::de::DeserializeOwned,
    {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map or JSON/YAML string map")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(BTreeMap::new());
            }
            if value.starts_with('{') {
                return serde_yaml::from_str::<BTreeMap<String, T>>(value).map_err(E::custom);
            }
            Err(E::custom("expected a JSON/YAML map string"))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(TypedMapVisitor(std::marker::PhantomData))
}

pub(crate) fn deserialize_typed_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct TypedListVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T> Visitor<'de> for TypedListVisitor<T>
    where
        T: serde::de::DeserializeOwned,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a list or JSON/YAML string list")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(Vec::new());
            }
            if value.starts_with('[') {
                return serde_yaml::from_str::<Vec<T>>(value).map_err(E::custom);
            }
            Err(E::custom("expected a JSON/YAML list string"))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<T>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(TypedListVisitor(std::marker::PhantomData))
}

pub(crate) fn parse_string_list(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with('[') {
        return serde_yaml::from_str::<Vec<String>>(value).unwrap_or_default();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn parse_string_map(value: &str) -> Result<BTreeMap<String, String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(BTreeMap::new());
    }
    if value.starts_with('{') {
        let value = serde_yaml::from_str::<serde_yaml::Value>(value).map_err(|e| e.to_string())?;
        return parse_yaml_string_map(value);
    }

    let mut map = BTreeMap::new();
    for entry in value.split([',', '&']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, value) = entry
            .split_once('=')
            .or_else(|| entry.split_once(':'))
            .ok_or_else(|| format!("invalid key/value entry `{entry}`"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err("map key must not be empty".to_string());
        }
        map.insert(key.to_string(), value.trim().to_string());
    }
    Ok(map)
}

fn parse_yaml_string_map(value: serde_yaml::Value) -> Result<BTreeMap<String, String>, String> {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            let mut values = BTreeMap::new();
            for (key, value) in mapping {
                let key = key
                    .as_str()
                    .ok_or_else(|| "map key must be a string".to_string())?
                    .to_string();
                values.insert(key, yaml_scalar_to_string(value)?);
            }
            Ok(values)
        }
        serde_yaml::Value::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected map value, got {other:?}")),
    }
}

fn yaml_scalar_to_string(value: serde_yaml::Value) -> Result<String, String> {
    match value {
        serde_yaml::Value::Null => Ok(String::new()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::String(value) => Ok(value),
        other => Err(format!("expected scalar map value, got {other:?}")),
    }
}

pub(crate) fn request_header(session: &pingora::prelude::Session, name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

pub(crate) fn best_prefix<'a, T>(
    values: &'a BTreeMap<String, T>,
    path: &str,
) -> Option<(&'a String, &'a T)> {
    values
        .iter()
        .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
}
