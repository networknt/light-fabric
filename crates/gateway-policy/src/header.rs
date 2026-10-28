use crate::config_util::{
    best_prefix, deserialize_string_list, deserialize_string_map, deserialize_typed_map,
};
use crate::{HeaderWriter, Request};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HEADER_FILE: &str = "header.yml";
pub const HEADER_MODULE_ID: &str = "light-pingora/header";
pub const HEADER_CONFIG_NAME: &str = "header";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub request: HeaderMutation,
    #[serde(default)]
    pub response: HeaderMutation,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub path_prefix_header: BTreeMap<String, HeaderPathPrefixConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderMutation {
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub remove: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub update: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderPathPrefixConfig {
    #[serde(default)]
    pub request: HeaderMutation,
    #[serde(default)]
    pub response: HeaderMutation,
}

pub fn apply_header_request<R: Request>(
    session: &mut R,
    config: &HeaderConfig,
    request_path: &str,
) -> Result<(), R::Error> {
    if !config.enabled {
        return Ok(());
    }
    apply_request_mutation(session, &config.request)?;
    if let Some((_, prefix_config)) = best_prefix(&config.path_prefix_header, request_path) {
        apply_request_mutation(session, &prefix_config.request)?;
    }
    Ok(())
}

pub fn apply_header_response<W: HeaderWriter>(
    response: &mut W,
    config: &HeaderConfig,
    request_path: &str,
) -> Result<(), W::Error> {
    if !config.enabled {
        return Ok(());
    }
    apply_response_mutation(response, &config.response)?;
    if let Some((_, prefix_config)) = best_prefix(&config.path_prefix_header, request_path) {
        apply_response_mutation(response, &prefix_config.response)?;
    }
    Ok(())
}

fn apply_request_mutation<R: Request>(
    session: &mut R,
    mutation: &HeaderMutation,
) -> Result<(), R::Error> {
    let headers = session.req_header_mut();
    for name in &mutation.remove {
        headers.remove_header(name.as_str());
    }
    for (name, value) in &mutation.update {
        headers.insert_header(name, value)?;
    }
    Ok(())
}

fn apply_response_mutation<W: HeaderWriter>(
    response: &mut W,
    mutation: &HeaderMutation,
) -> Result<(), W::Error> {
    for name in &mutation.remove {
        response.remove_header(name.as_str());
    }
    for (name, value) in &mutation.update {
        response.insert_header(name, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_config_accepts_string_shortcuts() {
        let config: HeaderConfig = serde_yaml::from_str(
            r#"
enabled: true
request:
  remove: a,b
  update: 'x-one=1,x-two=2'
pathPrefixHeader:
  /api:
    response:
      update:
        x-api: true
"#,
        )
        .expect("parse header config");

        assert_eq!(config.request.remove, ["a", "b"]);
        assert_eq!(config.request.update["x-one"], "1");
        assert_eq!(
            config
                .path_prefix_header
                .get("/api")
                .expect("path config")
                .response
                .update["x-api"],
            "true"
        );
    }
}
