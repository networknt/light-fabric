use super::support::*;
use serde_json::json;

#[tokio::test]
async fn built_app_secondary_diagnostic_never_refreshes_twice_or_authenticates() {
    for (case, code, fetches) in [
        ("forged", "ERR10003", 2),
        ("expired", "ERR10004", 1),
        ("issuer", "ERR10003", 1),
        ("audience", "ERR10003", 1),
    ] {
        let keys = Keys::new("resident");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks()), Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        use_jwt(dir.path());
        update_values(dir.path(), "security.enableVerifyScope", json!(true));
        update_values(dir.path(), "security.jwksRefreshMinIntervalSecs", json!(0));
        let primary = keys.token(claims("test-only-primary"));
        let mut secondary_claims = claims("test-only-secondary");
        match case {
            "expired" => secondary_claims["exp"] = json!(now_ms() / 1000 - 120),
            "issuer" => secondary_claims["iss"] = json!("wrong-issuer"),
            "audience" => secondary_claims["aud"] = json!("wrong-audience"),
            _ => {}
        }
        let secondary = if case == "forged" {
            Keys::new("resident").token(secondary_claims)
        } else {
            keys.token(secondary_claims)
        };
        let mut request = event("/x", "GET", Some(&primary));
        request["multiValueHeaders"]["x-scope-token"] = json!([format!("Bearer {secondary}")]);
        let runtime = Runtime::start(
            vec![Invocation::new(&format!("wp13-diagnostic-{case}"), request)],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        result.save(
            &format!("WP13-diagnostic-{case}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
        assert_posts(&runtime, 1);
        assert_eq!(invoke.count(), 0, "{case}");
        assert_eq!(
            jwks.count(),
            fetches,
            "{case}: bootstrap plus normal verification only"
        );
        let response = runtime.records()[0].json();
        assert_eq!(response["statusCode"], 401);
        assert!(
            response["body"].as_str().unwrap().contains(code),
            "{case}: {response}"
        );
        for token in [&primary, &secondary] {
            assert!(!result.stdout.contains(token));
            assert!(!result.stderr.contains(token));
        }
    }
}
