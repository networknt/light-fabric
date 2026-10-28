use super::*;
struct NoBind;

#[tokio::test]
async fn bounded_sources_survive_bootstrap_runtime_and_rule_stage_mutation() {
    static EMBEDDED: &[EmbeddedConfigFile] = &[EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    }];
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("values.yml"), "marker: original\n").unwrap();
    fs::write(dir.path().join("rule.yml"), "message: ${marker}\n").unwrap();
    let sources =
        config_loader::BoundedLocalSources::capture(EMBEDDED, None, dir.path(), dir.path())
            .unwrap();
    fs::write(dir.path().join("values.yml"), "[malformed").unwrap();
    fs::write(dir.path().join("rule.yml"), "[malformed").unwrap();
    let runtime = LightRuntimeBuilder::new(NoBind)
        .with_config_dir(dir.path())
        .with_external_config_dir(dir.path())
        .with_embedded_config(EMBEDDED)
        .build();
    let config = runtime
        .prepare_local_config_from_sources(&sources, false)
        .await
        .unwrap();
    assert_eq!(config.resolved_values["marker"].as_str(), Some("original"));
    fs::remove_file(dir.path().join("values.yml")).unwrap();
    fs::remove_file(dir.path().join("rule.yml")).unwrap();
    let rule = config
        .module_registry
        .load_config_bounded_from_sources(&config, &sources)
        .unwrap();
    assert_eq!(rule["message"].as_str(), Some("original"));
}
#[async_trait]
impl TransportRuntime for NoBind {
    type Handle = ();
    async fn bind(
        &self,
        _: &RuntimeConfig,
        _: &crate::LifecycleRegistrar,
        _: &crate::AdmissionGate,
        _: CancellationToken,
    ) -> Result<BoundTransport<()>, RuntimeError> {
        panic!("pure preparation must not bind")
    }
    async fn stop(&self, _: &mut (), _: &ShutdownContext) -> Result<(), RuntimeError> {
        panic!("pure preparation must not stop")
    }
}
#[tokio::test]
async fn explicit_local_entry_is_the_only_csv_admission_path() {
    static EMBEDDED: &[EmbeddedConfigFile] = &[EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    }];
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        "directUrls:\n  service: http://one:80,http://two:80\n",
    )
    .unwrap();
    let runtime = || {
        LightRuntimeBuilder::new(NoBind)
            .with_config_dir(dir.path())
            .with_external_config_dir(dir.path())
            .with_embedded_config(EMBEDDED)
            .build()
    };
    assert!(runtime().prepare_local_config().await.is_err());
    assert!(runtime().prepare_config().await.is_err());
    let config = runtime().prepare_local_config_for_router().await.unwrap();
    assert_eq!(
        config.direct_registry.direct_urls["service"],
        "http://one:80,http://two:80"
    );
    assert!(config.reload_context().await.is_err());
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        "directUrls:\n  service: http://one:80\n",
    )
    .unwrap();
    let legacy = runtime().prepare_local_config().await.unwrap();
    let router = runtime().prepare_local_config_for_router().await.unwrap();
    assert_eq!(
        legacy.direct_registry.direct_urls,
        router.direct_registry.direct_urls
    );
}
#[test]
fn router_csv_validates_every_member_without_relaxing_legacy_mode() {
    for raw in [
        "http://one:80,http://two:80",
        "http://one:80, https://two:443/base?fixed=1",
    ] {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([("service".into(), raw.into())]),
        };
        assert!(validate_direct_registry_with_mode(&config, RegistryAdmission::RouterCsv).is_ok());
        assert!(validate_direct_registry_config(&config).is_err());
    }
    for raw in [
        "http://one:80,",
        ",http://two:80",
        "http://one:80,ftp://two",
        "http://one:80,not-a-url",
        "http://user:pass@host:80",
        "http://host:80#fragment",
    ] {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([("service".into(), raw.into())]),
        };
        assert!(
            validate_direct_registry_with_mode(&config, RegistryAdmission::RouterCsv).is_err(),
            "{raw}"
        );
    }
    for raw in [" http://one:80/base?fixed=1 ", "https://host:443/"] {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([("service".into(), raw.into())]),
        };
        assert!(validate_direct_registry_config(&config).is_ok());
        assert!(validate_direct_registry_with_mode(&config, RegistryAdmission::RouterCsv).is_ok());
    }
}
