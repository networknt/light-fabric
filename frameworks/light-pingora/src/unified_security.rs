use crate::policy_adapter::PolicyRequest;
use crate::{
    apikey::ApiKeyConfig,
    basic_auth::BasicAuthConfig,
    hmac::HmacRuntime,
    security::{HandlerRejection, SecurityRuntime},
};
pub use gateway_policy::unified_security::*;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
pub fn load_unified_security_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<UnifiedSecurityConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<UnifiedSecurityConfig>(runtime_config, UNIFIED_SECURITY_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == UNIFIED_SECURITY_FILE => {
            UnifiedSecurityConfig::default()
        }
        Err(error) => return Err(error),
    };

    validate_unified_security_config(&config, None)?;

    runtime_config.module_registry.register_loaded_config(
        UNIFIED_SECURITY_MODULE_ID,
        UNIFIED_SECURITY_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub async fn verify_unified_security(
    session: &mut Session,
    config: &UnifiedSecurityConfig,
    basic_config: Option<&BasicAuthConfig>,
    api_key_config: Option<&ApiKeyConfig>,
    security_runtime: Option<&SecurityRuntime>,
    request_path: &str,
    request_method: &str,
) -> Result<UnifiedSecurityOutcome, HandlerRejection> {
    gateway_policy::unified_security::verify_unified_security(
        &mut PolicyRequest(session),
        config,
        basic_config,
        api_key_config,
        security_runtime,
        request_path,
        request_method,
    )
    .await
}
struct HmacValidationView<'a>(&'a HmacRuntime);
impl HmacValidation for HmacValidationView<'_> {
    fn standalone_routes(&self) -> Vec<HmacRoute> {
        self.0
            .standalone_routes()
            .into_iter()
            .map(|route| HmacRoute {
                prefix: route.prefix,
                methods: route.methods,
                profile: route.profile,
            })
            .collect()
    }
    fn contains_profile(&self, profile: &str) -> bool {
        self.0.contains_profile(profile)
    }
    fn standalone_policy_overlaps(
        &self,
        prefix: &str,
        methods: &[String],
    ) -> Result<bool, RuntimeError> {
        self.0.standalone_policy_overlaps(prefix, methods)
    }
}
pub fn validate_unified_security_config(
    config: &UnifiedSecurityConfig,
    hmac: Option<&HmacRuntime>,
) -> Result<(), RuntimeError> {
    let view = hmac.map(HmacValidationView);
    gateway_policy::unified_security::validate_unified_security_config(
        config,
        view.as_ref().map(|v| v as &dyn HmacValidation),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hmac::{HmacConfig, HmacSecretResolver};
    use base64::Engine as _;

    struct TestSecretResolver;

    impl HmacSecretResolver for TestSecretResolver {
        fn resolve(&self, _environment_name: &str) -> Result<Vec<u8>, RuntimeError> {
            Ok(b"test-secret".to_vec())
        }
    }

    fn hmac_runtime(standalone: bool) -> HmacRuntime {
        let standalone_rule = if standalone {
            r#"
pathPrefixAuths:
  - prefix: /standalone
    methods: [POST]
    profile: github
"#
        } else {
            ""
        };
        let config: HmacConfig = serde_yaml::from_str(
            format!(
                r#"
{standalone_rule}
profiles:
  github:
    secrets:
      defaultEnvNames: [TEST_SECRET]
"#
            )
            .as_str(),
        )
        .expect("parse HMAC config");
        HmacRuntime::compile(&config, &TestSecretResolver).expect("compile HMAC runtime")
    }

    // ---------------------------------------------------------------------------
    // Config parsing
    // ---------------------------------------------------------------------------

    #[test]
    fn unified_security_accepts_java_style_lists() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes: /health,/info
pathPrefixAuths: '[{"prefix":"/api","jwt":true},{"prefix":"/admin","basic":true}]'
"#,
        )
        .expect("parse unified config");

        assert!(is_anonymous(&config, "/health"));
        assert!(best_auth_rule(&config, "/api/pets", "GET").unwrap().jwt);
        assert!(
            best_auth_rule(&config, "/admin/users", "GET")
                .unwrap()
                .basic
        );
    }

    #[test]
    fn path_prefix_auth_parses_jwk_service_ids() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /salesforce
    jwt: true
    jwkServiceIds:
      - com.networknt.oauth2-salesforce-1.0.0
  - prefix: /internal
    jwt: true
    sjwt: true
    jwkServiceIds:
      - com.networknt.oauth2-internal-1.0.0
    sjwkServiceIds:
      - com.networknt.oauth2-simple-1.0.0
"#,
        )
        .expect("parse config with jwkServiceIds");

        let sf_rule = best_auth_rule(&config, "/salesforce/data", "GET").unwrap();
        assert!(sf_rule.jwt);
        assert_eq!(
            sf_rule.jwk_service_ids,
            ["com.networknt.oauth2-salesforce-1.0.0"]
        );

        let int_rule = best_auth_rule(&config, "/internal/api", "GET").unwrap();
        assert!(int_rule.jwt && int_rule.sjwt);
        assert_eq!(
            int_rule.jwk_service_ids,
            ["com.networknt.oauth2-internal-1.0.0"]
        );
        assert_eq!(
            int_rule.sjwk_service_ids,
            ["com.networknt.oauth2-simple-1.0.0"]
        );
    }

    #[test]
    fn path_prefix_auth_parses_jwk_service_ids_comma_format() {
        // Java-style comma-separated string in JSON array format
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths: '[{"prefix":"/sf","jwt":true,"jwkServiceIds":"svc1,svc2"}]'
"#,
        )
        .expect("parse JSON-string config");

        let rule = best_auth_rule(&config, "/sf/foo", "GET").unwrap();
        assert_eq!(rule.jwk_service_ids, ["svc1", "svc2"]);
    }

    // ---------------------------------------------------------------------------
    // Prefix matching
    // ---------------------------------------------------------------------------

    #[test]
    fn best_auth_rule_selects_longest_prefix() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /api
    jwt: true
  - prefix: /api/v2
    basic: true
"#,
        )
        .expect("parse");

        // /api/v2/pets should match /api/v2, not /api
        let rule = best_auth_rule(&config, "/api/v2/pets", "GET").unwrap();
        assert!(rule.basic, "longest prefix /api/v2 should win");

        // /api/v1/pets should fall through to /api
        let rule = best_auth_rule(&config, "/api/v1/pets", "GET").unwrap();
        assert!(rule.jwt, "/api should match /api/v1/pets");
    }

    #[test]
    fn best_auth_rule_returns_none_for_unmatched_path() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /api
    jwt: true
"#,
        )
        .expect("parse");

        assert!(best_auth_rule(&config, "/other/path", "GET").is_none());
    }

    #[test]
    fn method_aware_all_of_selects_hmac_profile() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
  - prefix: /partner
    methods: [GET]
    jwt: true
"#,
        )
        .expect("parse composed config");
        let runtime = hmac_runtime(false);
        validate_unified_security_config(&config, Some(&runtime)).expect("validate allOf");
        assert!(
            best_auth_rule(&config, "/partner/event", "POST")
                .unwrap()
                .requires_hmac()
        );
        assert_eq!(
            config.hmac_profile_for("/partner/event", "POST"),
            Some("github")
        );
        assert_eq!(config.hmac_profile_for("/partner/event", "GET"), None);
        assert!(
            best_auth_rule(&config, "/partner/event", "GET")
                .unwrap()
                .jwt
        );
    }

    #[test]
    fn rejects_unknown_hmac_profile_and_standalone_overlap() {
        let unknown: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: missing
"#,
        )
        .expect("parse unknown profile");
        assert!(validate_unified_security_config(&unknown, Some(&hmac_runtime(false))).is_err());

        let overlap: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /standalone/specific
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse overlapping policy");
        assert!(validate_unified_security_config(&overlap, Some(&hmac_runtime(true))).is_err());
    }

    #[test]
    fn rejects_both_method_fallthrough_directions() {
        let ancestor_fallthrough: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    jwt: true
  - prefix: /webhook/github
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse ancestor fallthrough");
        assert!(
            validate_unified_security_config(&ancestor_fallthrough, Some(&hmac_runtime(false)))
                .is_err()
        );

        let specific_shadow: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
  - prefix: /webhook/github
    methods: [POST]
    apikey: true
"#,
        )
        .expect("parse specific shadow");
        assert!(
            validate_unified_security_config(&specific_shadow, Some(&hmac_runtime(false))).is_err()
        );
    }

    #[test]
    fn rejects_fallthrough_between_standalone_hmac_and_legacy_unified_rules() {
        let hmac_config: HmacConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github
    methods: [POST]
    profile: github
profiles:
  github:
    secrets:
      defaultEnvNames: [TEST_SECRET]
"#,
        )
        .expect("parse standalone HMAC config");
        let hmac = HmacRuntime::compile(&hmac_config, &TestSecretResolver)
            .expect("compile standalone HMAC runtime");

        let ancestor: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    jwt: true
"#,
        )
        .expect("parse legacy ancestor");
        assert!(
            validate_unified_security_config(&ancestor, Some(&hmac)).is_err(),
            "an all-method legacy ancestor must not capture methods omitted by standalone HMAC"
        );

        let covered_ancestor: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    jwt: true
"#,
        )
        .expect("parse method-covered legacy ancestor");
        validate_unified_security_config(&covered_ancestor, Some(&hmac))
            .expect("a legacy ancestor restricted to HMAC-covered methods is valid");

        let specific: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github/private
    methods: [POST]
    apikey: true
"#,
        )
        .expect("parse legacy specific rule");
        assert!(
            validate_unified_security_config(&specific, Some(&hmac)).is_err(),
            "a more-specific legacy rule must not shadow standalone HMAC"
        );

        let same_prefix: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github
    methods: [POST]
    jwt: true
"#,
        )
        .expect("parse same-prefix legacy rule");
        assert!(
            validate_unified_security_config(&same_prefix, Some(&hmac)).is_err(),
            "equal-prefix policies from different entry points must not overlap"
        );
    }

    #[test]
    fn rejects_mixed_legacy_and_all_of_or_anonymous_overlap() {
        let mixed: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    jwt: true
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse mixed rule");
        assert!(validate_unified_security_config(&mixed, Some(&hmac_runtime(false))).is_err());

        let anonymous: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes: [/partner/health]
pathPrefixAuths:
  - prefix: /partner
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse anonymous overlap");
        assert!(validate_unified_security_config(&anonymous, Some(&hmac_runtime(false))).is_err());
    }

    #[test]
    fn anonymous_prefix_matches_correctly() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes:
  - /health
  - /server/info
pathPrefixAuths:
  - prefix: /
    jwt: true
"#,
        )
        .expect("parse");

        assert!(is_anonymous(&config, "/health"));
        assert!(is_anonymous(&config, "/health/live"));
        assert!(is_anonymous(&config, "/server/info"));
        assert!(!is_anonymous(&config, "/api/data"));
    }

    // ---------------------------------------------------------------------------
    // token_has_scope helper
    // ---------------------------------------------------------------------------

    /// Build a minimal unsigned JWT with the given payload for testing purposes.
    fn make_jwt_payload(payload_json: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.fakesig")
    }

    #[test]
    fn token_has_scope_detects_string_scope() {
        let token = make_jwt_payload(r#"{"sub":"user","scope":"read:api write:api"}"#);
        assert!(token_has_scope(&token));
    }

    #[test]
    fn token_is_jwt_like_requires_decodable_jwt() {
        let token = make_jwt_payload(r#"{"sub":"user"}"#);

        assert!(token_is_jwt_like(&token));
        assert!(!token_is_jwt_like("opaque-token"));
        assert!(!token_is_jwt_like("not.a.jwt"));
    }

    #[test]
    fn token_has_scope_detects_array_scp() {
        let token = make_jwt_payload(r#"{"sub":"user","scp":["read","write"]}"#);
        assert!(token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_empty_scope() {
        let token = make_jwt_payload(r#"{"sub":"user","scope":""}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_when_no_scope_claim() {
        let token = make_jwt_payload(r#"{"sub":"user","client_id":"app"}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_empty_array() {
        let token = make_jwt_payload(r#"{"sub":"user","scp":[]}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_invalid_token() {
        assert!(!token_has_scope("not.a.jwt"));
        assert!(!token_has_scope(""));
        assert!(!token_has_scope("onlyonepart"));
    }
}
