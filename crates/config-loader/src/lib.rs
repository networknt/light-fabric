use arc_swap::ArcSwap;
use asymmetric_decryptor::AsymmetricDecryptor;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde_yaml::{Mapping, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use symmetric_decryptor::{Decryptor as SymmetricDecryptorTrait, SymmetricDecryptor};
use thiserror::Error;
use tracing::{error, warn};

const EXTERNAL_CONFIG_DIR_ENV: &str = "LIGHT_RS_CONFIG_DIR";
const MAX_EXPANSION_DEPTH: usize = 16;
static WHOLE_VARIABLE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\$\{([^}:]+)(?::(.*))?\}$").expect("whole variable regex"));

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
    #[error("embedded config error: {0}")]
    Embedded(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedConfigFile {
    pub name: &'static str,
    pub content: &'static str,
}

pub fn embedded_config_file<'a>(
    embedded: &'a [EmbeddedConfigFile],
    file_name: &str,
) -> Option<&'a EmbeddedConfigFile> {
    embedded.iter().find(|file| file.name == file_name)
}

pub fn load_config_from_sources(
    loader: &ConfigLoader,
    embedded: &[EmbeddedConfigFile],
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: Option<&Path>,
    file_name: &str,
) -> Result<Option<Value>, ConfigError> {
    if let Some(path) = highest_priority_config_path(
        default_config_dir,
        config_dir,
        external_config_dir,
        file_name,
    ) {
        let mut value = loader.load_file(path)?;
        loader.resolve_value(&mut value)?;
        return Ok(Some(value));
    }

    let Some(file) = embedded_config_file(embedded, file_name) else {
        return Ok(None);
    };
    let mut value = loader.load_embedded_file(file)?;
    loader.resolve_value(&mut value)?;
    Ok(Some(value))
}

pub fn load_values_from_sources(
    embedded: &[EmbeddedConfigFile],
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: Option<&Path>,
    remote_values_yaml: Option<&str>,
) -> Result<HashMap<String, Value>, ConfigError> {
    let mut values = HashMap::new();

    if let Some(file) = embedded_config_file(embedded, VALUES_FILE_NAME) {
        values.extend(parse_values_content(file.content)?);
    }

    for path in existing_config_paths(
        default_config_dir,
        config_dir,
        external_config_dir,
        VALUES_FILE_NAME,
    ) {
        let content = fs::read_to_string(path)?;
        values.extend(parse_values_content(&content)?);
    }

    if let Some(remote_values_yaml) = remote_values_yaml {
        values.extend(parse_values_content(remote_values_yaml)?);
    }

    Ok(values)
}

pub fn handle_embedded_config_cli(embedded: &[EmbeddedConfigFile]) -> Result<bool, ConfigError> {
    handle_embedded_config_args(embedded, env::args().skip(1))
}

pub fn handle_embedded_config_args<I, S>(
    embedded: &[EmbeddedConfigFile],
    args: I,
) -> Result<bool, ConfigError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if let Some(index) = args.iter().position(|arg| arg == "--print-default-config") {
        let Some(file_name) = args.get(index + 1) else {
            return Err(ConfigError::Embedded(
                "--print-default-config requires a file name".to_string(),
            ));
        };
        print_embedded_config(embedded, file_name)?;
        return Ok(true);
    }
    if let Some(index) = args.iter().position(|arg| arg == "--dump-default-configs") {
        let Some(target_dir) = args.get(index + 1) else {
            return Err(ConfigError::Embedded(
                "--dump-default-configs requires a target directory".to_string(),
            ));
        };
        dump_embedded_configs(embedded, Path::new(target_dir))?;
        return Ok(true);
    }
    Ok(false)
}

pub fn print_embedded_config(
    embedded: &[EmbeddedConfigFile],
    file_name: &str,
) -> Result<(), ConfigError> {
    let Some(file) = embedded_config_file(embedded, file_name) else {
        return Err(ConfigError::Embedded(format!(
            "embedded config `{file_name}` was not found"
        )));
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(file.content.as_bytes())?;
    if !file.content.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

pub fn dump_embedded_configs(
    embedded: &[EmbeddedConfigFile],
    target_dir: &Path,
) -> Result<(), ConfigError> {
    for file in embedded {
        let target = target_dir.join(file.name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, file.content)?;
    }
    Ok(())
}

fn highest_priority_config_path(
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: Option<&Path>,
    file_name: &str,
) -> Option<PathBuf> {
    existing_config_paths(
        default_config_dir,
        config_dir,
        external_config_dir,
        file_name,
    )
    .into_iter()
    .last()
}

pub fn existing_config_paths(
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: Option<&Path>,
    file_name: &str,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(default_config_dir) = default_config_dir {
        push_existing_config_path(&mut paths, default_config_dir, file_name);
    }
    push_existing_config_path(&mut paths, config_dir, file_name);
    if let Some(external_config_dir) = external_config_dir {
        push_existing_config_path(&mut paths, external_config_dir, file_name);
    }

    paths
}

fn push_existing_config_path(paths: &mut Vec<PathBuf>, dir: &Path, file_name: &str) {
    let path = dir.join(file_name);
    if path.exists() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn parse_values_content(content: &str) -> Result<HashMap<String, Value>, ConfigError> {
    if content.trim().is_empty() {
        return Ok(HashMap::new());
    }
    Ok(serde_yaml::from_str(content)?)
}

const VALUES_FILE_NAME: &str = "values.yml";

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

    pub fn load_embedded_file(&self, file: &EmbeddedConfigFile) -> Result<Value, ConfigError> {
        Self::parse_config_str(Path::new(file.name), file.content)
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
        self.resolve_value_with_depth(value, 0)
    }

    fn resolve_value_with_depth(&self, value: &mut Value, depth: usize) -> Result<(), ConfigError> {
        if depth >= MAX_EXPANSION_DEPTH {
            return Err(ConfigError::Convert(format!(
                "nested value resolution exceeded max depth {MAX_EXPANSION_DEPTH} while resolving value: {:?}",
                value
            )));
        }
        match value {
            Value::String(s) => {
                if let Some(resolved_value) = self.resolve_whole_variable_reference(s, depth)? {
                    *value = resolved_value;
                } else {
                    let resolved_str = self.expand_variables(s)?;
                    *value = self.resolve_scalar_value(resolved_str);
                }
            }
            Value::Mapping(map) => {
                for (_, v) in map.iter_mut() {
                    self.resolve_value_with_depth(v, depth)?;
                }
            }
            Value::Sequence(seq) => {
                for v in seq.iter_mut() {
                    self.resolve_value_with_depth(v, depth)?;
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

    fn resolve_whole_variable_reference(
        &self,
        input: &str,
        depth: usize,
    ) -> Result<Option<Value>, ConfigError> {
        let Some(caps) = WHOLE_VARIABLE_REGEX.captures(input) else {
            return Ok(None);
        };
        let key = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let default = caps.get(2).map(|m| m.as_str());

        if let Some(env_val) = self.get_env_value(key) {
            return Ok(Some(self.resolve_scalar_from_text(env_val, true)));
        }

        if let Some(value) = self.values.get(key) {
            let mut resolved = value.clone();
            self.resolve_value_with_depth(&mut resolved, depth + 1)?;
            return Ok(Some(resolved));
        }

        if let Some(default_value) = default {
            return Ok(Some(
                self.resolve_scalar_from_text(default_value.to_string(), true),
            ));
        }

        Err(ConfigError::UnresolvedVariable(input.to_string()))
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
        if let Some(env_val) = self.get_env_value(key) {
            return Some(env_val);
        }

        self.values.get(key).and_then(Self::scalar_to_string)
    }

    fn get_env_value(&self, key: &str) -> Option<String> {
        let normalized = key.to_uppercase().replace(['-', '.'], "_");
        if let Ok(env_val) = env::var(&normalized) {
            return Some(env_val);
        }
        if let Ok(env_val) = env::var(key) {
            return Some(env_val);
        }
        None
    }

    pub fn parse_config_str(path: impl AsRef<Path>, content: &str) -> Result<Value, ConfigError> {
        let path = path.as_ref();
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

    fn resolve_scalar_value(&self, val: String) -> Value {
        self.resolve_scalar_from_text(val, false)
    }

    fn resolve_scalar_from_text(&self, val: String, parse_scalar: bool) -> Value {
        let decrypted = if val.starts_with("CRYPT:RSA:") {
            if let Some(ref ad) = self.asymmetric_decryptor {
                match ad.decrypt(&val) {
                    Ok(decrypted) => decrypted,
                    Err(e) => {
                        error!("Asymmetric decryption failed: {:?}", e);
                        val
                    }
                }
            } else {
                warn!("Found asymmetric secret but no private key provided.");
                val
            }
        } else if val.starts_with("CRYPT:") {
            if let Some(ref sd) = self.symmetric_decryptor {
                match sd.decrypt(&val) {
                    Ok(decrypted) => decrypted,
                    Err(e) => {
                        error!("Symmetric decryption failed: {:?}", e);
                        val
                    }
                }
            } else {
                warn!("Found symmetric secret but no password provided.");
                val
            }
        } else {
            val
        };
        if parse_scalar {
            Self::parse_text_scalar(&decrypted)
        } else {
            Value::String(decrypted)
        }
    }

    fn parse_text_scalar(input: &str) -> Value {
        let trimmed = input.trim();
        if matches!(trimmed.as_bytes().first(), Some(b'[' | b'{')) {
            if let Ok(value) = serde_yaml::from_str::<Value>(trimmed) {
                return value;
            }
        }
        if trimmed.eq_ignore_ascii_case("true") {
            return Value::Bool(true);
        }
        if trimmed.eq_ignore_ascii_case("false") {
            return Value::Bool(false);
        }
        if let Ok(parsed) = trimmed.parse::<i64>() {
            return serde_yaml::to_value(parsed)
                .unwrap_or_else(|_| Value::String(input.to_string()));
        }
        if let Ok(parsed) = trimmed.parse::<f64>() {
            if parsed.is_finite() {
                return serde_yaml::to_value(parsed)
                    .unwrap_or_else(|_| Value::String(input.to_string()));
            }
        }
        Value::String(input.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    use serde::Deserialize;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_TEST_MUTEX: Mutex<()> = Mutex::new(());

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
        let loader =
            ConfigLoader::new("host: example.com\nbase_url: https://${host}\n", None, None)
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

        let loader =
            ConfigLoader::new("host: example.com\nbase_url: https://${host}\n", None, None)
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

        let loader =
            ConfigLoader::new("base_url: https://typed.example\n", None, None).expect("loader");
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

    #[test]
    fn preserves_expanded_bool_like_strings_as_strings() {
        let loader = ConfigLoader::new(
            "feature_flag: \"true\"\nport: \"001\"\nscientific: \"1e3\"\n",
            None,
            None,
        )
        .expect("loader");

        let mut bool_value = Value::String("${feature_flag}".to_string());
        let mut number_value = Value::String("${port}".to_string());
        let mut scientific_value = Value::String("${scientific}".to_string());

        loader.resolve_value(&mut bool_value).expect("resolve bool");
        loader
            .resolve_value(&mut number_value)
            .expect("resolve number");
        loader
            .resolve_value(&mut scientific_value)
            .expect("resolve scientific");

        assert_eq!(bool_value, Value::String("true".to_string()));
        assert_eq!(number_value, Value::String("001".to_string()));
        assert_eq!(scientific_value, Value::String("1e3".to_string()));
    }

    #[test]
    fn whole_variable_expansion_preserves_typed_values() {
        let loader = ConfigLoader::new("enabled: true\nport: 8083\n", None, None).expect("loader");

        let mut bool_value = Value::String("${enabled:false}".to_string());
        let mut number_value = Value::String("${port:8081}".to_string());

        loader.resolve_value(&mut bool_value).expect("resolve bool");
        loader
            .resolve_value(&mut number_value)
            .expect("resolve number");

        assert_eq!(bool_value, Value::Bool(true));
        assert_eq!(
            number_value,
            serde_yaml::to_value(8083).expect("yaml number")
        );
    }

    #[test]
    fn whole_variable_default_values_are_typed() {
        let loader = ConfigLoader::new("", None, None).expect("loader");

        let mut bool_value = Value::String("${enabled:false}".to_string());
        let mut number_value = Value::String("${port:8081}".to_string());
        let mut sequence_value = Value::String("${items:[]}".to_string());
        let mut mapping_value = Value::String("${settings:{}}".to_string());

        loader.resolve_value(&mut bool_value).expect("resolve bool");
        loader
            .resolve_value(&mut number_value)
            .expect("resolve number");
        loader
            .resolve_value(&mut sequence_value)
            .expect("resolve sequence");
        loader
            .resolve_value(&mut mapping_value)
            .expect("resolve mapping");

        assert_eq!(bool_value, Value::Bool(false));
        assert_eq!(
            number_value,
            serde_yaml::to_value(8081).expect("yaml number")
        );
        assert_eq!(sequence_value, Value::Sequence(Vec::new()));
        assert_eq!(mapping_value, Value::Mapping(Mapping::new()));
    }

    #[test]
    fn multi_variable_strings_fall_back_to_regular_expansion() {
        let loader =
            ConfigLoader::new("host: example.com\nport: 8443\n", None, None).expect("loader");
        let mut value = Value::String("${host}:${port}".to_string());

        loader.resolve_value(&mut value).expect("resolve value");

        assert_eq!(value, Value::String("example.com:8443".to_string()));
    }

    #[test]
    fn whole_variable_env_values_are_decrypted_before_parsing() {
        let _guard = ENV_TEST_MUTEX.lock().expect("env test mutex");
        let salt = hex::decode("ebfab3ef4261185776a026acf72d24ee").expect("salt");
        let key = {
            let mut key = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", &salt, 65536, &mut key);
            key
        };
        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        let encryptor = Aes256CbcEnc::new(&key.into(), &[0u8; 16].into());
        let mut buf = b"false".to_vec();
        let msg_len = buf.len();
        buf.resize(msg_len + 16, 0);
        let ciphertext = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
            .expect("encrypt")
            .to_vec();
        let encrypted = format!("CRYPT:{}:{}", hex::encode(salt), hex::encode(ciphertext));
        unsafe {
            env::set_var("ENCRYPTED_FLAG", encrypted);
        }

        let loader = ConfigLoader::new("", Some("password"), None).expect("loader");
        let mut value = Value::String("${ENCRYPTED_FLAG}".to_string());
        let resolved = loader.resolve_value(&mut value);

        unsafe {
            env::remove_var("ENCRYPTED_FLAG");
        }

        resolved.expect("resolve value");
        assert_eq!(value, Value::Bool(false));
    }

    #[test]
    fn dotted_keys_can_be_overridden_by_shell_friendly_env_vars() {
        let _guard = ENV_TEST_MUTEX.lock().expect("env test mutex");
        unsafe {
            env::set_var("SERVER_SERVICEID", "com.networknt.agent.demo-1.0.0");
        }

        let loader = ConfigLoader::new("", None, None).expect("loader");
        let mut value =
            Value::String("${server.serviceId:com.networknt.agent.account-1.0.0}".to_string());
        let resolved = loader.resolve_value(&mut value);

        unsafe {
            env::remove_var("SERVER_SERVICEID");
        }

        resolved.expect("resolve value");
        assert_eq!(
            value,
            Value::String("com.networknt.agent.demo-1.0.0".to_string())
        );
    }

    #[test]
    fn structured_config_uses_highest_priority_source_without_deep_merge() {
        let embedded = [EmbeddedConfigFile {
            name: "server.yml",
            content: "ip: 0.0.0.0\nhttpPort: 8080\nserviceId: embedded\n",
        }];
        let config_dir = TempDir::new("config-source");
        fs::write(config_dir.path().join("server.yml"), "serviceId: local\n")
            .expect("write local config");
        let loader = ConfigLoader::new("", None, None).expect("loader");

        let value = load_config_from_sources(
            &loader,
            &embedded,
            None,
            config_dir.path(),
            None,
            "server.yml",
        )
        .expect("load config")
        .expect("config value");

        let map = value.as_mapping().expect("mapping");
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get(&Value::String("serviceId".to_string()))
                .expect("serviceId"),
            &Value::String("local".to_string())
        );
    }

    #[test]
    fn values_merge_from_embedded_files_and_remote_payload() {
        let embedded = [EmbeddedConfigFile {
            name: "values.yml",
            content: "a: embedded\nb: embedded\n",
        }];
        let config_dir = TempDir::new("values-source");
        fs::write(config_dir.path().join("values.yml"), "b: local\n").expect("write local values");

        let values = load_values_from_sources(
            &embedded,
            None,
            config_dir.path(),
            None,
            Some("b: remote\nc: true\n"),
        )
        .expect("load values");

        assert_eq!(values["a"], Value::String("embedded".to_string()));
        assert_eq!(values["b"], Value::String("remote".to_string()));
        assert_eq!(values["c"], Value::Bool(true));
    }

    #[test]
    fn self_referential_values_fail_instead_of_recursing_forever() {
        let loader = ConfigLoader::new("key: \"${key}\"\n", None, None).expect("loader");
        let mut value = Value::String("${key}".to_string());

        let error = loader
            .resolve_value(&mut value)
            .expect_err("cycle should fail");

        assert!(matches!(error, ConfigError::Convert(_)));
    }
}
