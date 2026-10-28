//! Opt-in local source-capture qualification, not a production admission profile.
use super::*;
use config_loader::{ConfigLoader, EmbeddedConfigFile};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

/// Explicit experimental raw capture bounds. Expanded documents additionally use
/// the existing config-loader bounded profile; production admission remains gated.
pub struct CaptureLimits {
    pub per_source: usize,
    pub aggregate: usize,
}

pub struct CapturedRouterSources {
    values: Vec<Arc<[u8]>>,
    files: BTreeMap<&'static str, Arc<[u8]>>,
    identity: String,
    bytes: usize,
    resolved: Option<(serde_json::Value, BTreeMap<String, String>)>,
}
impl CapturedRouterSources {
    pub fn capture(
        embedded: &[EmbeddedConfigFile],
        default: Option<&Path>,
        local: &Path,
        external: &Path,
        limits: CaptureLimits,
    ) -> Result<Self, InitError> {
        let mut used = 0usize;
        let mut hash = Sha256::new();
        hash.update(b"WP12/router-source-capture/v1\0");
        let mut capture = |name: &str,
                           path: Option<&Path>,
                           content: Option<&str>|
         -> Result<Arc<[u8]>, InitError> {
            let remaining = limits
                .aggregate
                .checked_sub(used)
                .ok_or_else(invalid)?
                .min(limits.per_source);
            let bytes: Arc<[u8]> = if let Some(path) = path {
                let mut file = std::fs::File::open(path).map_err(|_| invalid())?;
                let bound = remaining.checked_add(1).ok_or_else(invalid)?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(bound).map_err(|_| invalid())?;
                let mut scratch = [0u8; 8192];
                while bytes.len() < bound {
                    let count = scratch.len().min(bound - bytes.len());
                    let n = file.read(&mut scratch[..count]).map_err(|_| invalid())?;
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&scratch[..n]);
                }
                if bytes.len() > remaining {
                    return Err(invalid());
                }
                bytes.into()
            } else {
                let content = content.ok_or_else(invalid)?;
                if content.len() > remaining {
                    return Err(invalid());
                }
                content.as_bytes().into()
            };
            used += bytes.len();
            hash.update((name.len() as u64).to_be_bytes());
            hash.update(name.as_bytes());
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(&bytes);
            Ok(bytes)
        };
        let mut values = Vec::new();
        if let Some(file) = config_loader::embedded_config_file(embedded, "values.yml") {
            values.push(capture("values.yml", None, Some(file.content))?);
        }
        for path in
            config_loader::existing_config_paths(default, local, Some(external), "values.yml")
        {
            values.push(capture("values.yml", Some(&path), None)?);
        }
        let mut files = BTreeMap::new();
        for name in ["router.yml", "service.yml", "direct-registry.yml"] {
            if let Some(path) =
                config_loader::existing_config_paths(default, local, Some(external), name).last()
            {
                files.insert(name, capture(name, Some(path), None)?);
            } else if let Some(file) = config_loader::embedded_config_file(embedded, name) {
                files.insert(name, capture(name, None, Some(file.content))?);
            }
        }
        // No parser or substitution runs until all participating captures succeed.
        let mut captured = Self {
            values,
            files,
            identity: format!("{:x}", hash.finalize()),
            bytes: used,
            resolved: None,
        };
        captured.resolved = Some(captured.resolve_sources()?);
        Ok(captured)
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub(super) fn resolve(
        &self,
    ) -> Result<(serde_json::Value, BTreeMap<String, String>), InitError> {
        self.resolved.clone().ok_or_else(invalid)
    }
    fn resolve_sources(&self) -> Result<(serde_json::Value, BTreeMap<String, String>), InitError> {
        let values =
            ConfigLoader::parse_captured_values_bounded(&self.values).map_err(|_| invalid())?;
        let password = std::env::var("light_4j_config_password").ok();
        // Resolution borrows the captured values; do not clone the whole map into
        // the loader or invoke the legacy unbounded resolver.
        let loader = ConfigLoader::from_values(Default::default(), password.as_deref(), None)
            .map_err(|_| invalid())?;
        let load = |name: &str| -> Result<Option<serde_json::Value>, InitError> {
            self.files
                .get(name)
                .map(|raw| {
                    let mut value =
                        ConfigLoader::parse_captured_config_bounded(raw).map_err(|_| invalid())?;
                    loader
                        .resolve_value_bounded(&mut value, &values)
                        .map_err(|_| invalid())?;
                    serde_json::to_value(value).map_err(|_| invalid())
                })
                .transpose()
        };
        let router = load("router.yml")?.ok_or_else(invalid)?;
        let service = load("service.yml")?.unwrap_or_else(|| serde_json::json!({}));
        let direct = load("direct-registry.yml")?
            .map(serde_json::from_value::<light_runtime::config::DirectRegistryConfig>)
            .transpose()
            .map_err(|_| invalid())?
            .unwrap_or_default();
        Ok((router, destinations_value(&service, &direct.direct_urls)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> CaptureLimits {
        CaptureLimits {
            per_source: 1 << 20,
            aggregate: 4 << 20,
        }
    }
    #[test]
    fn capture_precedence_values_service_and_mutation_are_immutable() {
        static EMBEDDED: &[EmbeddedConfigFile] = &[
            EmbeddedConfigFile {
                name: "values.yml",
                content: "destination: http://embedded:80\n",
            },
            EmbeddedConfigFile {
                name: "router.yml",
                content: "enabled: false\n",
            },
        ];
        let root = tempfile::tempdir().unwrap();
        let dirs = [
            root.path().join("default"),
            root.path().join("local"),
            root.path().join("external"),
        ];
        for dir in &dirs {
            std::fs::create_dir(dir).unwrap();
        }
        for (i, dir) in dirs.iter().enumerate() {
            std::fs::write(
                dir.join("values.yml"),
                format!("destination: http://stage-{i}:80\n"),
            )
            .unwrap();
            std::fs::write(
                dir.join("router.yml"),
                format!("enabled: false\nmaxRequestTime: {}\n", i + 1),
            )
            .unwrap();
        }
        std::fs::write(
            dirs[2].join("direct-registry.yml"),
            "directUrls:\n  service: ${destination}\n",
        )
        .unwrap();
        let captured =
            CapturedRouterSources::capture(EMBEDDED, Some(&dirs[0]), &dirs[1], &dirs[2], limits())
                .unwrap();
        let identity = captured.identity().to_owned();
        let (router, urls) = captured.resolve().unwrap();
        assert_eq!(router["maxRequestTime"], 3);
        assert_eq!(urls["service"], "http://stage-2:80");
        for dir in &dirs {
            std::fs::write(dir.join("values.yml"), "destination: http://mutated:80\n").unwrap();
        }
        std::fs::remove_file(dirs[2].join("direct-registry.yml")).unwrap();
        assert_eq!(captured.resolve().unwrap(), (router, urls));
        assert_eq!(captured.identity(), identity);
        std::fs::write(dirs[2].join("service.yml"),"singletons:\n- com.networknt.registry.URL:\n  - com.networknt.registry.URLImpl:\n      parameters:\n        service: http://service:80\n").unwrap();
        let next =
            CapturedRouterSources::capture(EMBEDDED, Some(&dirs[0]), &dirs[1], &dirs[2], limits())
                .unwrap();
        assert_ne!(next.identity(), identity);
        assert_eq!(next.resolve().unwrap().1["service"], "http://service:80");
    }
    #[test]
    fn raw_boundaries_are_checked_for_disabled_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        let raw = "enabled: false\n";
        std::fs::write(dir.path().join("router.yml"), raw).unwrap();
        let capture = |per_source, aggregate| {
            CapturedRouterSources::capture(
                &[],
                None,
                &missing,
                dir.path(),
                CaptureLimits {
                    per_source,
                    aggregate,
                },
            )
        };
        assert_eq!(capture(raw.len(), raw.len()).unwrap().bytes(), raw.len());
        assert!(capture(raw.len() - 1, raw.len()).is_err());
        assert!(capture(raw.len(), raw.len() - 1).is_err());
        std::fs::write(dir.path().join("service.yml"), "[malformed").unwrap();
        assert!(capture(1024, 2048).is_err());
    }

    #[test]
    fn captured_expansion_is_bounded_without_reopening_sources() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        std::fs::write(dir.path().join("router.yml"), "enabled: false\n").unwrap();
        std::fs::write(
            dir.path().join("direct-registry.yml"),
            "directUrls:\n  service: ${destination}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("values.yml"),
            "host: example.test\ndestination: http://${host}:80\n",
        )
        .unwrap();
        let captured =
            CapturedRouterSources::capture(&[], None, &missing, dir.path(), limits()).unwrap();
        assert_eq!(
            captured.resolve().unwrap().1["service"],
            "http://example.test:80"
        );

        // Both sources fit the raw cap, but the resolved document does not fit
        // the bounded effective-document profile, even for disabled Router.
        std::fs::write(
            dir.path().join("values.yml"),
            format!("destination: http://example.test/{}\n", "x".repeat(65536)),
        )
        .unwrap();
        assert!(CapturedRouterSources::capture(&[], None, &missing, dir.path(), limits()).is_err());
        assert_eq!(
            captured.resolve().unwrap().1["service"],
            "http://example.test:80"
        );
    }
}
