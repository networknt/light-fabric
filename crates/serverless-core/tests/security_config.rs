use serde_json::json;
use serverless_core::{handlers::security::UnifiedSettings, unsupported::validate_security};

#[test]
fn production_policy_and_exact_test_exception_do_not_relax_skew() {
    for flag in ["ignoreJwtExpiry", "enableMockJwt", "logJwtToken"] {
        let mut v = json!({});
        v[flag] = json!(true);
        assert!(validate_security(&v, false).is_err(), "{flag}");
        assert!(
            validate_security(&v, true).is_ok(),
            "explicit exception: {flag}"
        );
    }
    for skew in [
        json!(61),
        json!(120),
        json!(-1),
        json!(1.5),
        json!("60"),
        json!(null),
    ] {
        for allow in [false, true] {
            assert!(validate_security(&json!({"jwt":{"clockSkewInSeconds":skew}}), allow).is_err());
        }
    }
    for skew in [0, 30, 60] {
        assert!(validate_security(&json!({"jwt":{"clockSkewInSeconds":skew}}), false).is_ok());
    }
}

#[test]
fn unknown_malformed_and_nondefault_deferred_security_settings_fail_init_validation() {
    for value in [
        json!({"unknown":false}),
        json!({"jwt":{"unknown":false}}),
        json!({"jwt":null}),
        json!({"enabled":"false"}),
        json!({"enableVerifyJwt":null}),
        json!({"enableVerifyScope":"true"}),
        json!({"enableVerifySwt":true}),
        json!({"enableRelaxedKeyValidation":true}),
        json!({"enableH2c":true}),
        json!({"logClientUserScope":true}),
        json!({"passThroughClaims":{"role":"x-role"}}),
        json!({"providerId":"unsupported"}),
        json!({"jwt":{"keyResolver":"X509Certificate"}}),
        json!({"jwt":{"certificate":{"kid":"file.pem"}}}),
        json!({"skipPathPrefixes":[42]}),
        json!({"swtClientIdHeader":"other"}),
    ] {
        assert!(validate_security(&value, false).is_err(), "{value}");
    }
    assert!(validate_security(&json!({"jwt":{"keyResolver":"JsonWebKeySet","certificate":{}},"enableVerifySwt":false}),false).is_ok());
    assert!(
        !validate_security(&json!({"enabled":false}), false)
            .unwrap()
            .enabled
    );
}

#[test]
fn unified_enabled_unsupported_factors_are_never_ignored_even_on_unused_or_disabled_paths() {
    for factor in ["swt", "sjwt", "ldap"] {
        for enabled in [false, true] {
            let mut entry = json!({"prefix":"/unused","jwt":true});
            entry[factor] = json!(true);
            assert!(
                UnifiedSettings::from_value(&json!({"enabled":enabled,"pathPrefixAuths":[entry]}))
                    .is_err(),
                "{factor}"
            );
        }
        let mut entry = json!({"prefix":"/x","jwt":true});
        entry[factor] = json!(false);
        assert!(UnifiedSettings::from_value(&json!({"pathPrefixAuths":[entry]})).is_ok());
    }
}

#[test]
fn unified_unknown_malformed_factors_and_disabled_factor_nondefaults_fail_validation() {
    for value in [
        json!({"unknown":false}),
        json!({"pathPrefixAuths":{}}),
        json!({"enabled":null}),
        json!({"anonymousPrefixes":[1]}),
        json!({"pathPrefixAuths":[{"prefix":"/x","jwt":"true"}]}),
        json!({"pathPrefixAuths":[{"prefix":"/x","unknown":false}]}),
        json!({"pathPrefixAuths":[{"jwt":true}]}),
        json!({"pathPrefixAuths":[{"prefix":"/x","swt":false,"swtServiceIds":["swt"]}]}),
        json!({"pathPrefixAuths":[{"prefix":"/x","jwt":false,"jwkServiceIds":["jwt"]}]}),
    ] {
        assert!(UnifiedSettings::from_value(&value).is_err(), "{value}");
    }
    assert!(UnifiedSettings::from_value(&json!({"anonymousPrefixes":"/public,/health","pathPrefixAuths":"[{\"prefix\":\"/x\",\"jwt\":true,\"jwkServiceIds\":\"a,b\"}]"})).is_ok());
}
