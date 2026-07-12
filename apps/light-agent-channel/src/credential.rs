use serde::Deserialize;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialEntry {
    connector_alias: String,
    token_file: PathBuf,
}

#[derive(Clone)]
pub struct ConnectorCredentialStore {
    entries: BTreeMap<String, ResolvedCredential>,
}

#[derive(Clone)]
struct ResolvedCredential {
    connector_alias: String,
    token: String,
}

impl ConnectorCredentialStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let metadata = fs::metadata(&path)
            .map_err(|e| format!("connector credential map is unavailable: {e}"))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("connector credential map must be an owner-only regular file".into());
        }
        let configured: BTreeMap<String, CredentialEntry> = serde_json::from_slice(
            &fs::read(&path).map_err(|e| format!("read connector credential map: {e}"))?,
        )
        .map_err(|e| format!("parse connector credential map: {e}"))?;
        if configured.is_empty() {
            return Err("connector credential map cannot be empty".into());
        }
        let mut entries = BTreeMap::new();
        for (reference, entry) in configured {
            if reference.is_empty()
                || entry.connector_alias.is_empty()
                || !entry.token_file.is_absolute()
            {
                return Err("connector credential entries require a reference, alias, and absolute token file".into());
            }
            let token_metadata = fs::metadata(&entry.token_file)
                .map_err(|e| format!("connector credential {reference} is unavailable: {e}"))?;
            if !token_metadata.is_file() || token_metadata.permissions().mode() & 0o077 != 0 {
                return Err(format!(
                    "connector credential {reference} must be an owner-only regular file"
                ));
            }
            let token = fs::read_to_string(&entry.token_file)
                .map_err(|e| format!("read connector credential {reference}: {e}"))?
                .trim()
                .to_string();
            if token.is_empty() {
                return Err(format!("connector credential {reference} is empty"));
            }
            entries.insert(
                reference,
                ResolvedCredential {
                    connector_alias: entry.connector_alias,
                    token,
                },
            );
        }
        Ok(Self { entries })
    }

    pub fn bearer(&self, reference: &str, connector_alias: &str) -> Result<&str, String> {
        self.secret(reference, connector_alias)
    }

    pub fn secret(&self, reference: &str, connector_alias: &str) -> Result<&str, String> {
        let entry = self
            .entries
            .get(reference)
            .ok_or_else(|| "connector credential reference is not configured".to_string())?;
        if entry.connector_alias != connector_alias {
            return Err("connector credential alias does not match the grant".into());
        }
        Ok(&entry.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn reference_and_connector_alias_are_both_required() {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("slack.token");
        fs::write(&token, "secret\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let map = directory.path().join("credentials.json");
        fs::write(
            &map,
            format!(
                r#"{{"grant/slack":{{"connectorAlias":"slack-api-v1","tokenFile":"{}"}}}}"#,
                token.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&map, fs::Permissions::from_mode(0o600)).unwrap();

        let store = ConnectorCredentialStore::load(map).unwrap();
        assert_eq!(
            store.bearer("grant/slack", "slack-api-v1").unwrap(),
            "secret"
        );
        assert!(store.bearer("grant/slack", "github-api-v1").is_err());
        assert!(store.bearer("grant/other", "slack-api-v1").is_err());
    }
}
