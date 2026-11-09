use super::*;
use crate::msal_auth::policy_tests::jwks_server;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::json;
use std::sync::atomic::Ordering;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const CLIENT: &str = "33333333-3333-3333-3333-333333333333";
const KEYS: &str = include_str!("../tests/fixtures/msal/signing-key.jwks.json");
const OTHER_KEYS: &str = include_str!("../tests/fixtures/msal/other-signing-key.jwks.json");
const KEY: &[u8] = include_bytes!("../tests/fixtures/msal/signing-key.pem");
const OTHER: &[u8] = include_bytes!("../tests/fixtures/msal/other-signing-key.pem");

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn policy(role: TokenRole, uri: &str) -> SpaTokenPolicy {
    SpaTokenPolicy {
        issuer: if role == TokenRole::Internal {
            "https://internal.example".into()
        } else {
            format!("https://login.microsoftonline.com/{TENANT}/v2.0")
        },
        audience: if role == TokenRole::Internal {
            "portal".into()
        } else {
            CLIENT.into()
        },
        jwks_uri: uri.into(),
        tenant_id: TENANT.into(),
        required_scopes: vec!["access_as_user".into()],
        allowed_client_ids: vec![CLIENT.into()],
        ..Default::default()
    }
}
fn claims(role: TokenRole) -> Value {
    let p = policy(role, "https://fixture.example/keys");
    let mut claims = json!({"iss":p.issuer,"aud":p.audience,"sub":"user","exp":now()+3600});
    if role == TokenRole::Internal {
        claims["csrf"] = json!("signed-context");
    } else {
        claims["tid"] = json!(TENANT);
        claims["ver"] = json!("2.0");
        if role == TokenRole::MicrosoftId {
            claims["nonce"] = json!("client-owned-nonce");
        } else {
            claims["scp"] = json!("access_as_user");
            claims["azp"] = json!(CLIENT);
        }
    }
    claims
}
fn sign(claims: &Value, key: &[u8]) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("msal-test".into());
    encode(&header, claims, &EncodingKey::from_rsa_pem(key).unwrap()).unwrap()
}

#[tokio::test]
async fn internal_trust_is_isolated_from_foreign_same_kid_and_generic_principal_cache() {
    let first = jwks_server(KEYS).await;
    let second = jwks_server(OTHER_KEYS).await;
    let mut a =
        SpaTokenTrust::new(policy(TokenRole::Internal, &first.uri), TokenRole::Internal).unwrap();
    Arc::get_mut(&mut a.source).unwrap().client = first.client;
    let mut foreign_policy = policy(TokenRole::Internal, &second.uri);
    foreign_policy.issuer = "https://foreign.example".into();
    let mut b = SpaTokenTrust::new(foreign_policy, TokenRole::Internal).unwrap();
    Arc::get_mut(&mut b.source).unwrap().client = second.client;
    let valid = sign(&claims(TokenRole::Internal), KEY);
    let mut foreign_claims = claims(TokenRole::Internal);
    foreign_claims["iss"] = json!("https://foreign.example");
    let foreign = sign(&foreign_claims, OTHER);
    for foreign_first in [false, true] {
        if foreign_first {
            b.verify(&foreign, false).await.unwrap();
        }
        a.verify(&valid, false).await.unwrap();
        assert_eq!(a.verify(&foreign, false).await.unwrap_err().status, 401);
        b.verify(&foreign, false).await.unwrap();
        assert_eq!(b.verify(&valid, false).await.unwrap_err().status, 401);
    }
    let mut generic =
        crate::security::SecurityRuntime::with_test_hs256_key("msal-test", b"generic-only-secret")
            .await;
    generic.config.enable_jwt_cache = true;
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("msal-test".into());
    let token = encode(
        &header,
        &claims(TokenRole::Internal),
        &EncodingKey::from_secret(b"generic-only-secret"),
    )
    .unwrap();
    crate::security::verify_jwt_token(&generic, &token, crate::security::JwtExpiryMode::Enforce)
        .await
        .unwrap();
    assert_eq!(a.verify(&token, false).await.unwrap_err().status, 401);
    // A warm source never caches the resulting principal or bypasses a changed claim policy.
    a.policy.audience = "another-application".into();
    assert_eq!(a.verify(&valid, false).await.unwrap_err().status, 401);
}

#[tokio::test]
async fn microsoft_id_and_api_roles_cannot_be_confused_even_with_the_same_audience_and_signer() {
    let server = jwks_server(KEYS).await;
    let mut id = SpaTokenTrust::new(
        policy(TokenRole::MicrosoftId, &server.uri),
        TokenRole::MicrosoftId,
    )
    .unwrap();
    Arc::get_mut(&mut id.source).unwrap().client = server.client.clone();
    let api = SpaTokenTrust {
        policy: policy(TokenRole::MicrosoftApi, &server.uri),
        role: TokenRole::MicrosoftApi,
        source: id.source.clone(),
    };
    for _ in 0..2 {
        let id_token = sign(&claims(TokenRole::MicrosoftId), KEY);
        let access_token = sign(&claims(TokenRole::MicrosoftApi), KEY);
        id.verify(&id_token, false).await.unwrap();
        api.verify(&access_token, false).await.unwrap();
        assert_eq!(
            id.verify(&access_token, false).await.unwrap_err().status,
            401
        );
        assert_eq!(api.verify(&id_token, false).await.unwrap_err().status, 401);
        assert_eq!(
            api.verify_for(TokenRole::Internal, &access_token, false)
                .await
                .unwrap_err()
                .status,
            401
        );
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    for role in [TokenRole::MicrosoftId, TokenRole::MicrosoftApi] {
        let trust = if role == TokenRole::MicrosoftId {
            &id
        } else {
            &api
        };
        for (field, value) in [
            ("iss", json!("https://foreign.example")),
            ("aud", json!("wrong")),
            ("tid", json!(CLIENT)),
            ("ver", json!("1.0")),
            ("exp", json!(now() - 1)),
            ("nbf", json!(now() + 3600)),
            ("token_use", json!("wrong-purpose")),
            ("token_use", json!(42)),
        ] {
            let mut bad = claims(role);
            bad[field] = value;
            assert_eq!(
                trust
                    .verify(&sign(&bad, KEY), false)
                    .await
                    .unwrap_err()
                    .status,
                401,
                "{field}"
            );
        }
    }
    let mut invalid = claims(TokenRole::MicrosoftApi);
    invalid["scp"] = json!("access_as_user_extra");
    assert!(api.verify(&sign(&invalid, KEY), false).await.is_err());
    invalid = claims(TokenRole::MicrosoftApi);
    invalid["azp"] = json!(TENANT);
    assert!(api.verify(&sign(&invalid, KEY), false).await.is_err());
    invalid = claims(TokenRole::MicrosoftId);
    invalid.as_object_mut().unwrap().remove("nonce");
    assert!(id.verify(&sign(&invalid, KEY), false).await.is_err());
}

#[tokio::test]
async fn selected_internal_policy_is_also_used_for_expired_logout_tokens() {
    let trust = SpaTokenTrust::test_internal();
    let mut expired = claims(TokenRole::Internal);
    expired["exp"] = json!(now() - 3600);
    let token = sign(&expired, KEY);
    trust.verify(&token, true).await.unwrap();
    assert_eq!(trust.verify(&token, false).await.unwrap_err().status, 401);
    expired["aud"] = json!("unrelated");
    assert_eq!(
        trust
            .verify(&sign(&expired, KEY), true)
            .await
            .unwrap_err()
            .status,
        401
    );
    expired = claims(TokenRole::Internal);
    expired["nonce"] = json!("id-token");
    assert_eq!(
        trust
            .verify(&sign(&expired, KEY), false)
            .await
            .unwrap_err()
            .status,
        401
    );
    for purpose in [json!("app"), json!("id_token"), json!(42)] {
        let mut wrong = claims(TokenRole::Internal);
        wrong["token_use"] = purpose;
        assert_eq!(
            trust
                .verify(&sign(&wrong, KEY), false)
                .await
                .unwrap_err()
                .status,
            401
        );
    }
}

#[tokio::test]
async fn microsoft_v1_api_uses_appid_and_its_configured_application_uri() {
    let mut p = policy(TokenRole::MicrosoftApi, "https://fixture.example/keys");
    p.issuer = format!("https://sts.windows.net/{TENANT}/");
    p.audience = "api://legacy-application".into();
    let trust = SpaTokenTrust::new(p.clone(), TokenRole::MicrosoftApi).unwrap();
    trust.source.seed_test_keys(KEYS);
    let mut payload = claims(TokenRole::MicrosoftApi);
    payload["iss"] = json!(p.issuer);
    payload["aud"] = json!(p.audience);
    payload["ver"] = json!("1.0");
    payload["appid"] = json!(CLIENT);
    payload.as_object_mut().unwrap().remove("azp");
    trust.verify(&sign(&payload, KEY), false).await.unwrap();
    payload["appid"] = json!(TENANT);
    assert_eq!(
        trust
            .verify(&sign(&payload, KEY), false)
            .await
            .unwrap_err()
            .status,
        401
    );
}

fn config(dir: &tempfile::TempDir) -> RuntimeConfig {
    RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: None,
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: dir.path().into(),
        external_config_dir: dir.path().join("external"),
        resolved_values: BTreeMap::new().into_iter().collect(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Arc::new(light_runtime::ModuleRegistry::new()),
        cache_registry: None,
        registry_client: None,
    }
}

#[tokio::test]
async fn selected_role_startup_validation_and_replacement_reset_source_state() {
    let server = jwks_server(KEYS).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    assert!(load_spa_trust(&cfg, true, false, false).is_err());
    let policies = SpaTokenTrustConfig {
        internal: policy(TokenRole::Internal, &server.uri),
        microsoft_id: policy(TokenRole::MicrosoftId, &server.uri),
        microsoft_api: policy(TokenRole::MicrosoftApi, &server.uri),
        ..Default::default()
    };
    std::fs::write(
        dir.path().join(SPA_TOKEN_TRUST_FILE),
        serde_yaml::to_string(&policies).unwrap(),
    )
    .unwrap();
    let first = load_spa_trust(&cfg, true, true, true).unwrap();
    first.register(&cfg).unwrap();
    let reload = cfg
        .module_registry
        .reload_modules(
            light_runtime::ReloadContext::new(cfg.clone()),
            &[SPA_TOKEN_TRUST_MODULE_ID.to_owned()],
        )
        .await;
    assert!(reload.reloaded.is_empty());
    assert_eq!(reload.skipped[0].reason, "requiresRestart");
    assert!(Arc::ptr_eq(
        &first.id.as_ref().unwrap().source,
        &first.api.as_ref().unwrap().source
    ));
    let first = first.internal.unwrap();
    first.source.seed_test_keys(KEYS);
    let replacement = load_spa_trust(&cfg, true, false, false)
        .unwrap()
        .internal
        .unwrap();
    assert!(!Arc::ptr_eq(&first.source, &replacement.source));
    // Replacement has no inherited fixture key or principal cache; untrusted TLS fails closed.
    assert_eq!(
        replacement
            .verify(&sign(&claims(TokenRole::Internal), KEY), false)
            .await
            .unwrap_err()
            .status,
        503
    );
    first
        .verify(&sign(&claims(TokenRole::Internal), KEY), false)
        .await
        .unwrap();
    std::fs::write(dir.path().join("statelessAuth.yml"), "enabled: false\n").unwrap();
    std::fs::write(dir.path().join("msal-exchange.yml"), "enabled: false\n").unwrap();
    std::fs::write(
        dir.path().join(SPA_TOKEN_TRUST_FILE),
        "not valid trust configuration",
    )
    .unwrap();
    let metadata = cfg.module_registry.component_configs();
    assert!(load_spa_trust(&cfg, true, false, false).is_err());
    assert_eq!(metadata, cfg.module_registry.component_configs());
    first
        .verify(&sign(&claims(TokenRole::Internal), KEY), false)
        .await
        .unwrap();
    assert!(
        crate::stateless_auth::load_stateless_auth_runtime(&cfg, true)
            .unwrap()
            .is_none()
    );
    assert!(
        crate::msal_exchange::load_msal_exchange_runtime(&cfg, true)
            .unwrap()
            .is_none()
    );
    let mut p = policy(TokenRole::MicrosoftApi, &server.uri);
    p.audience = "api://v2-uri-is-not-an-api-guid".into();
    assert!(p.validate(TokenRole::MicrosoftApi).is_err());
    p.issuer = format!("https://sts.windows.net/{TENANT}/");
    assert!(p.validate(TokenRole::MicrosoftApi).is_ok());
}

#[test]
fn shipped_trust_template_supplies_portal_internal_defaults_for_startup() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = shipped_template_config(&dir);
    let policies: SpaTokenTrustConfig = cfg
        .module_registry
        .load_config(&cfg, SPA_TOKEN_TRUST_FILE)
        .unwrap();
    assert!(policies.internal.issuer.is_empty());
    let defaults = policies.portal_internal_defaults.as_ref().unwrap();
    assert_eq!(defaults.issuer, "urn:com:networknt:oauth2:v1");
    assert_eq!(defaults.audience, "urn:com.networknt");
    assert_eq!(
        defaults.jwks_uri,
        "https://light-oauth:6881/oauth2/AZZRJE52eXu3t1hseacnGQ/keys"
    );
    assert_eq!(defaults.jwks_ca_paths, ["/config/ca.pem"]);
    assert_eq!(defaults.clock_skew_seconds, 60);
    defaults.validate(TokenRole::Internal).unwrap();
    assert!(
        policies
            .microsoft_id
            .validate(TokenRole::MicrosoftId)
            .is_err()
    );
    assert!(
        policies
            .microsoft_api
            .validate(TokenRole::MicrosoftApi)
            .is_err()
    );

    // Retarget the preset's packaged CA into this fixture's filesystem. Do not
    // supply internal property overrides: those must disable the whole preset.
    let ca = dir.path().join("ca.pem");
    std::fs::write(
        &ca,
        include_str!("../../../apps/light-gateway/config/ca.pem"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(SPA_TOKEN_TRUST_FILE),
        include_str!("../../../apps/light-gateway/config/spa-token-trust.yml")
            .replace("/config/ca.pem", ca.to_str().unwrap()),
    )
    .unwrap();
    std::fs::write(dir.path().join("statelessAuth.yml"), "enabled: true\n").unwrap();
    std::fs::write(dir.path().join("client.yml"), "{}\n").unwrap();
    assert!(
        crate::stateless_auth::load_stateless_auth_runtime(&cfg, true)
            .unwrap()
            .is_some()
    );
    assert!(load_spa_trust(&cfg, true, true, false).is_err());
}

fn shipped_template_config(dir: &tempfile::TempDir) -> RuntimeConfig {
    std::fs::write(
        dir.path().join(SPA_TOKEN_TRUST_FILE),
        include_str!("../../../apps/light-gateway/config/spa-token-trust.yml"),
    )
    .unwrap();
    config(dir)
}

#[test]
fn shipped_trust_template_resolves_selected_role_values_without_unused_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = shipped_template_config(&dir);
    for (key, value) in [
        ("issuer", "https://internal.example"),
        ("audience", "portal"),
        ("jwksUri", "https://internal.example/keys"),
    ] {
        cfg.resolved_values.insert(
            format!("spa-token-trust.internal.{key}"),
            serde_yaml::Value::String(value.into()),
        );
    }
    let bundle = load_spa_trust(&cfg, true, false, false).unwrap();
    let policy = &bundle.internal.as_ref().unwrap().policy;
    assert_eq!(policy.issuer, "https://internal.example");
    assert_eq!(policy.audience, "portal");
    assert_eq!(policy.jwks_uri, "https://internal.example/keys");
    assert!(policy.jwks_ca_paths.is_empty());
    assert!(bundle.id.is_none());
    assert!(bundle.api.is_none());
    assert!(load_spa_trust(&cfg, true, true, false).is_err());
}

#[test]
fn shipped_trust_template_rejects_partial_and_misspelled_overrides() {
    for mask in 0..7 {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = shipped_template_config(&dir);
        // An empty explicit CA list must not hide a mixed identity policy.
        cfg.resolved_values.insert(
            "spa-token-trust.internal.jwksCaPaths".into(),
            serde_yaml::Value::String(String::new()),
        );
        for (index, (key, value)) in [
            ("issuer", "https://custom.example"),
            ("audience", "custom"),
            ("jwksUri", "https://custom.example/keys"),
        ]
        .into_iter()
        .enumerate()
        {
            if mask & (1 << index) != 0 {
                cfg.resolved_values.insert(
                    format!("spa-token-trust.internal.{key}"),
                    serde_yaml::Value::String(value.into()),
                );
            }
        }
        assert!(
            load_spa_trust(&cfg, true, false, false).is_err(),
            "mask={mask}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = shipped_template_config(&dir);
    cfg.resolved_values.insert(
        "spa-token-trust.internal.issure".into(),
        serde_yaml::Value::String("https://custom.example".into()),
    );
    cfg.resolved_values.insert(
        "spa-token-trust.internal.jwksCaPaths".into(),
        serde_yaml::Value::String(String::new()),
    );
    assert!(load_spa_trust(&cfg, true, false, false).is_err());
}

#[test]
fn shipped_trust_template_rejects_missing_preset_ca_and_invalid_explicit_ca() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = shipped_template_config(&dir);
    let missing = dir.path().join("missing-ca.pem");
    std::fs::write(
        dir.path().join(SPA_TOKEN_TRUST_FILE),
        include_str!("../../../apps/light-gateway/config/spa-token-trust.yml")
            .replace("/config/ca.pem", missing.to_str().unwrap()),
    )
    .unwrap();
    assert!(matches!(
        load_spa_trust(&cfg, true, false, false),
        Err(RuntimeError::Unsupported(message)) if message == "Cannot read SPA JWKS CA certificate"
    ));
    for (key, value) in [
        ("issuer", "https://custom.example"),
        ("audience", "custom"),
        ("jwksUri", "https://custom.example/keys"),
    ] {
        cfg.resolved_values.insert(
            format!("spa-token-trust.internal.{key}"),
            serde_yaml::Value::String(value.into()),
        );
    }
    // A full explicit authority uses public roots when CA is unset, regardless
    // of whether the unused preset's CA exists on this topology.
    assert!(load_spa_trust(&cfg, true, false, false).is_ok());
    cfg.resolved_values.insert(
        "spa-token-trust.internal.jwksCaPaths".into(),
        serde_yaml::Value::String(missing.to_str().unwrap().into()),
    );
    assert!(load_spa_trust(&cfg, true, false, false).is_err());
}

#[test]
fn literal_policy_typos_are_rejected_and_framework_has_no_implicit_preset() {
    assert!(serde_yaml::from_str::<SpaTokenTrustConfig>("internal:\n  issure: wrong\n").is_err());
    assert!(serde_yaml::from_str::<SpaTokenTrustConfig>("internl:\n  issuer: wrong\n").is_err());
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    std::fs::write(dir.path().join(SPA_TOKEN_TRUST_FILE), "internal: {}\n").unwrap();
    assert!(load_spa_trust(&cfg, true, false, false).is_err());
}
