use arc_swap::ArcSwap;
use asymmetric_decryptor::AsymmetricDecryptor;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde_yaml::{Mapping, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use symmetric_decryptor::{Decryptor as SymmetricDecryptorTrait, SymmetricDecryptor};
use thiserror::Error;
use tracing::{error, warn};

const EXTERNAL_CONFIG_DIR_ENV: &str = "LIGHT_RS_CONFIG_DIR";
const MAX_EXPANSION_DEPTH: usize = 16;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decryption error: {0}")]
    Decrypt(String),
    #[error("unsupported config format for {0}")]
    UnsupportedFormat(PathBuf),
    #[error("recursive variable expansion did not converge for `{0}`")]
    UnresolvedVariable(String),
    #[error("typed config conversion failed: {0}")]
    Convert(String),
}

pub struct ConfigManager<T> {
    current: ArcSwap<T>,
}

impl<T> ConfigManager<T> {
    pub fn new(initial: T) -> Self {
        Self {
            current: ArcSwap::from(Arc::new(initial)),
        }
    }

    pub fn get(&self) -> Arc<T> {
        self.current.load_full()
    }

    pub fn update(&self, new_config: T) {
        self.current.store(Arc::new(new_config));
    }
}

pub struct ConfigLoader {
    values: HashMap<String, Value>,
    symmetric_decryptor: Option<SymmetricDecryptor>,
    asymmetric_decryptor: Option<AsymmetricDecryptor>,
}

impl ConfigLoader {
    pub fn new(
        values_yaml: &str,
        password: Option<&str>,
        private_key_pem: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let values = if values_yaml.trim().is_empty() {
            HashMap::new()
        } else {
            serde_yaml::from_str(values_yaml)?
        };
        Self::from_values(values, password, private_key_pem)
    }

    pub fn from_values(
        values: HashMap<String, Value>,
        password: Option<&str>,
        private_key_pem: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let symmetric_decryptor = password.map(SymmetricDecryptor::new);
        let asymmetric_decryptor = private_key_pem
            .map(AsymmetricDecryptor::from_pem)
            .transpose()
            .map_err(|e| ConfigError::Decrypt(format!("{e:?}")))?;

        Ok(Self {
            values,
            symmetric_decryptor,
            asymmetric_decryptor,
        })
    }

    pub fn load_file(&self, path: impl AsRef<Path>) -> Result<Value, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)?;
        Self::parse_config_str(path, &content)
    }

    pub fn load_merged_files<I, P>(&self, paths: I) -> Result<Value, ConfigError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut merged = Value::Mapping(Mapping::new());
        for path in paths {
            let next = self.load_file(path)?;
            Self::merge_values(&mut merged, next);
        }
        self.resolve_value(&mut merged)?;
        Ok(merged)
    }

    pub fn load_layered_files<I, P>(&self, base_paths: I) -> Result<Value, ConfigError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut merged = Value::Mapping(Mapping::new());
        let external_dir = env::var(EXTERNAL_CONFIG_DIR_ENV).ok().map(PathBuf::from);

        for base_path in base_paths {
            let base_path = base_path.as_ref();
            let base_value = self.load_file(base_path)?;
            Self::merge_values(&mut merged, base_value);

            if let Some(ref external_dir) = external_dir {
                let external_path = external_dir.join(
                    base_path
                        .file_name()
                        .ok_or_else(|| ConfigError::UnsupportedFormat(base_path.to_path_buf()))?,
                );
                if external_path.exists() {
                    let external_value = self.load_file(&external_path)?;
                    Self::merge_values(&mut merged, external_value);
                }
            }
        }

        self.resolve_value(&mut merged)?;
        Ok(merged)
    }

    pub fn load_typed<T, I, P>(&self, base_paths: I) -> Result<T, ConfigError>
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let merged = self.load_layered_files(base_paths)?;
        serde_yaml::from_value(merged).map_err(|e| ConfigError::Convert(e.to_string()))
    }

    pub fn resolve_value(&self, value: &mut Value) -> Result<(), ConfigError> {
        match value {
            Value::String(s) => {
                let resolved_str = self.expand_variables(s)?;
                *value = self.auto_decrypt(resolved_str);
            }
            Value::Mapping(map) => {
                for (_, v) in map.iter_mut() {
                    self.resolve_value(v)?;
                }
            }
            Value::Sequence(seq) => {
                for v in seq.iter_mut() {
                    self.resolve_value(v)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn expand_variables(&self, input: &str) -> Result<String, ConfigError> {
        let re = Regex::new(r"\$\{([^}]+)\}").expect("variable regex");
        let mut current = input.to_string();

        for _ in 0..MAX_EXPANSION_DEPTH {
            let next = re
                .replace_all(&current, |caps: &regex::Captures<'_>| {
                    let inner = &caps[1];
                    let parts: Vec<&str> = inner.splitn(2, ':').collect();
                    let key = parts[0];
                    let default = parts.get(1).map(|v| (*v).to_string());
                    self.get_value(key)
                        .or(default)
                        .unwrap_or_else(|| caps[0].to_string())
                })
                .into_owned();

            if next == current {
                if re.is_match(&next) {
                    return Err(ConfigError::UnresolvedVariable(next));
                }
                return Ok(next);
            }
            current = next;
        }

        Err(ConfigError::UnresolvedVariable(current))
    }

    pub async fn fetch_remote_config(
        &self,
        url: &str,
        query: &HashMap<String, String>,
        auth_token: Option<&str>,
    ) -> Result<String, ConfigError> {
        let client = reqwest::Client::new();
        let mut request = client.get(url).query(query);

        if let Some(token) = auth_token {
            request = request.header("Authorization", token);
        }

        let response = request.send().await?;
        if response.status().is_success() {
            Ok(response.text().await?)
        } else {
            Err(ConfigError::Http(response.error_for_status().unwrap_err()))
        }
    }

    pub fn merge_values(base: &mut Value, overlay: Value) {
        match (base, overlay) {
            (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
                for (key, overlay_value) in overlay_map {
                    match base_map.get_mut(&key) {
                        Some(base_value) => Self::merge_values(base_value, overlay_value),
                        None => {
                            base_map.insert(key, overlay_value);
                        }
                    }
                }
            }
            (base_value, overlay_value) => *base_value = overlay_value,
        }
    }

    fn get_value(&self, key: &str) -> Option<String> {
        if let Ok(env_val) = env::var(key.to_uppercase().replace('-', "_")) {
            return Some(env_val);
        }
        if let Ok(env_val) = env::var(key) {
            return Some(env_val);
        }

        self.values.get(key).and_then(Self::scalar_to_string)
    }

    fn parse_config_str(path: &Path, content: &str) -> Result<Value, ConfigError> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("yaml" | "yml") => Ok(serde_yaml::from_str(content)?),
            Some("json") => Ok(serde_json::from_str(content)?),
            Some("toml") => {
                let toml_value: toml::Value = toml::from_str(content)?;
                Ok(serde_yaml::to_value(toml_value)?)
            }
            _ => Err(ConfigError::UnsupportedFormat(path.to_path_buf())),
        }
    }

    fn scalar_to_string(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    fn auto_decrypt(&self, val: String) -> Value {
        if val.starts_with("CRYPT:RSA:") {
            if let Some(ref ad) = self.asymmetric_decryptor {
                match ad.decrypt(&val) {
                    Ok(decrypted) => Value::String(decrypted),
                    Err(e) => {
                        error!("Asymmetric decryption failed: {:?}", e);
                        Value::String(val)
                    }
                }
            } else {
                warn!("Found asymmetric secret but no private key provided.");
                Value::String(val)
            }
        } else if val.starts_with("CRYPT:") {
            if let Some(ref sd) = self.symmetric_decryptor {
                match sd.decrypt(&val) {
                    Ok(decrypted) => Value::String(decrypted),
                    Err(e) => {
                        error!("Symmetric decryption failed: {:?}", e);
                        Value::String(val)
                    }
                }
            } else {
                warn!("Found symmetric secret but no password provided.");
                Value::String(val)
            }
        } else {
            Value::String(val)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let path = env::temp_dir().join(format!("light-rs-{label}-{suffix}"));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct ExampleConfig {
        service: ServiceConfig,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct ServiceConfig {
        url: String,
        enabled: bool,
    }

    #[test]
    fn resolves_nested_variables_recursively() {
        let loader = ConfigLoader::new(
            "host: example.com\nbase_url: https://${host}\n",
            None,
            None,
        )
        .expect("loader");
        let mut value = Value::String("${base_url}/api".to_string());

        loader.resolve_value(&mut value).expect("resolve value");

        assert_eq!(value, Value::String("https://example.com/api".to_string()));
    }

    #[test]
    fn merges_layered_files_and_external_overrides() {
        let base_dir = TempDir::new("base");
        let external_dir = TempDir::new("external");
        let base_path = base_dir.path().join("service.yml");
        let external_path = external_dir.path().join("service.yml");

        fs::write(
            &base_path,
            "service:\n  url: ${base_url}/v1\n  enabled: false\n  protocol: https\n",
        )
        .expect("write base");
        fs::write(&external_path, "service:\n  enabled: true\n").expect("write external");

        let old_external = env::var(EXTERNAL_CONFIG_DIR_ENV).ok();
        unsafe {
            env::set_var(EXTERNAL_CONFIG_DIR_ENV, external_dir.path());
        }

        let loader = ConfigLoader::new("host: example.com\nbase_url: https://${host}\n", None, None)
            .expect("loader");
        let merged = loader
            .load_layered_files([base_path.as_path()])
            .expect("load layered files");

        if let Some(previous) = old_external {
            unsafe {
                env::set_var(EXTERNAL_CONFIG_DIR_ENV, previous);
            }
        } else {
            unsafe {
                env::remove_var(EXTERNAL_CONFIG_DIR_ENV);
            }
        }

        assert_eq!(merged["service"]["enabled"], Value::Bool(true));
        assert_eq!(
            merged["service"]["url"],
            Value::String("https://example.com/v1".to_string())
        );
    }

    #[test]
    fn loads_typed_config_from_toml() {
        let dir = TempDir::new("toml");
        let path = dir.path().join("service.toml");
        fs::write(
            &path,
            "[service]\nurl = \"${base_url}/typed\"\nenabled = true\n",
        )
        .expect("write toml");

        let loader = ConfigLoader::new("base_url: https://typed.example\n", None, None)
            .expect("loader");
        let typed: ExampleConfig = loader.load_typed([path.as_path()]).expect("typed config");

        assert_eq!(
            typed,
            ExampleConfig {
                service: ServiceConfig {
                    url: "https://typed.example/typed".to_string(),
                    enabled: true,
                },
            }
        );
    }
}
