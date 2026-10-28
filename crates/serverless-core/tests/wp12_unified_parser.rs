//! Approved parser expansion only; runtime factor qualification is separate.
use serde_json::json;
use serverless_core::handlers::security::UnifiedSettings;

#[test]
fn approved_factors_preserve_each_prefix_and_combination_in_declared_order() {
    let settings = UnifiedSettings::from_value(&json!({
        "enabled": true,
        "pathPrefixAuths": [
            {"prefix":"/x", "basic":true},
            {"prefix":"/x/private", "apikey":true},
            {"prefix":"/jwt", "jwt":true,"jwkServiceIds":["provider-a","provider-b"]},
            {"prefix":"/mixed", "basic":true,"jwt":true,"apikey":true},
            {"prefix":"/off", "basic":false,"jwt":false,"apikey":false}
        ]
    }))
    .unwrap();
    assert!(settings.enabled);
    assert!(settings.anonymous.is_empty());
    let factors: Vec<_> = settings
        .prefixes
        .iter()
        .map(|p| (p.prefix.as_str(), p.basic, p.jwt, p.apikey))
        .collect();
    assert_eq!(
        factors,
        vec![
            ("/x", true, false, false),
            ("/x/private", false, false, true),
            ("/jwt", false, true, false),
            ("/mixed", true, true, true),
            ("/off", false, false, false)
        ]
    );
    assert_eq!(settings.prefixes[2].services, ["provider-a", "provider-b"]);
}

#[test]
fn stringified_prefix_table_retains_approved_flags_and_never_invents_anonymous_paths() {
    let settings = UnifiedSettings::from_value(&json!({"pathPrefixAuths":"[{\"prefix\":\"/x\",\"basic\":true,\"jwt\":true,\"apikey\":true,\"jwkServiceIds\":\"a,b\"}]"})).unwrap();
    let p = &settings.prefixes[0];
    assert!(p.basic && p.jwt && p.apikey);
    assert_eq!(p.services, ["a", "b"]);
    assert!(settings.anonymous.is_empty());
}

#[test]
fn supported_flags_are_strict_even_when_disabled_or_on_unused_paths() {
    for factor in ["basic", "apikey", "jwt"] {
        for enabled in [false, true] {
            for malformed in [json!(null), json!(1), json!("true"), json!([]), json!({})] {
                let mut entry = json!({"prefix":"/unused"});
                entry[factor] = malformed;
                assert!(
                    UnifiedSettings::from_value(
                        &json!({"enabled":enabled,"pathPrefixAuths":[entry]})
                    )
                    .is_err()
                );
            }
            let mut entry = json!({"prefix":"/unused"});
            entry[factor] = json!(true);
            assert!(
                UnifiedSettings::from_value(&json!({"enabled":enabled,"pathPrefixAuths":[entry]}))
                    .is_ok()
            );
        }
    }
}

#[test]
fn deferred_and_unknown_factors_remain_rejected_for_every_supported_combination() {
    for enabled in [false, true] {
        for factor in [
            "swt",
            "sjwt",
            "ldap",
            "hmac",
            "hmacProfile",
            "authentication",
            "unknown",
        ] {
            let mut entry = json!({"prefix":"/unused","basic":true,"jwt":true,"apikey":true});
            entry[factor] = json!(true);
            assert!(
                UnifiedSettings::from_value(&json!({"enabled":enabled,"pathPrefixAuths":[entry]}))
                    .is_err(),
                "{factor}"
            );
        }
        for key in ["swtServiceIds", "sjwkServiceIds"] {
            let mut entry = json!({"prefix":"/unused","basic":true,"jwt":true,"apikey":true});
            entry[key] = json!(["deferred-provider"]);
            assert!(
                UnifiedSettings::from_value(&json!({"enabled":enabled,"pathPrefixAuths":[entry]}))
                    .is_err()
            );
        }
    }
    for value in [
        json!({"pathPrefixAuths":[{"prefix":false,"basic":true}]}),
        json!({"pathPrefixAuths":[{"prefix":"bad","apikey":true}]}),
        json!({"pathPrefixAuths":[{"prefix":"/x","apikey":true,"jwkServiceIds":["provider"]}]}),
        json!({"unknown":false}),
        json!({"pathPrefixAuths":"not json"}),
    ] {
        assert!(UnifiedSettings::from_value(&value).is_err());
    }
}
