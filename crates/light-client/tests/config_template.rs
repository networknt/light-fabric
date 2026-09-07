use config_loader::ConfigLoader;
use light_client::ClientConfig;
use std::path::Path;

#[test]
fn shared_template_resolves_defaults_and_remote_oauth_settings() {
    let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/client.yml");
    let defaults: ClientConfig = ConfigLoader::new("", None, None)
        .unwrap()
        .load_typed([&template])
        .unwrap();
    assert_eq!(defaults.request, ClientConfig::default().request);
    assert!(defaults.tls.verify_hostname);
    assert_eq!(defaults.tls.tls_version, None);
    assert_eq!(defaults.oauth.token.early_refresh_retry_delay, 4000);
    assert_eq!(defaults.oauth.token.key.uri, "/oauth2/key");

    let values = r#"
client.tokenKeyServerUrl: https://oauth.example.test
client.tokenKeyUri: /oauth2/test/keys
client.tokenKeyServiceIdAuthServers:
  test-service:
    server_url: https://issuer.example.test
    uri: /keys
client.tokenCcServiceIdAuthServers:
  test-service:
    server_url: https://issuer.example.test
    uri: /token
client.tokenAcScope: [openid, profile]
client.verifyHostname: false
client.timeout: 9123
"#;
    let loader = ConfigLoader::new(values, None, None).unwrap();
    let fabric = template.parent().unwrap().join("../../..");
    let workspace = fabric.join("..");
    let targets: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(fabric.join("scripts/client-config-targets.json")).unwrap(),
    )
    .unwrap();
    for relative in targets.keys() {
        let path = workspace.join(relative);
        // Sibling deployment repositories are optional in a standalone checkout.
        if !workspace.join(relative.split('/').next().unwrap()).exists() {
            continue;
        }
        let config: ClientConfig = loader.load_typed([&path]).unwrap();
        assert_eq!(
            config.oauth.token.key.server_url.as_deref(),
            Some("https://oauth.example.test"),
            "{relative}"
        );
        assert_eq!(
            config.oauth.token.key.uri, "/oauth2/test/keys",
            "{relative}"
        );
        assert!(
            config
                .oauth
                .token
                .key
                .service_id_auth_servers
                .contains_key("test-service"),
            "{relative}"
        );
        assert!(
            config
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers
                .contains_key("test-service"),
            "{relative}"
        );
        assert_eq!(
            config.oauth.token.authorization_code.scope,
            ["openid", "profile"],
            "{relative}"
        );
        assert!(!config.tls.verify_hostname, "{relative}");
        assert_eq!(config.request.timeout, 9123, "{relative}");
    }
}
