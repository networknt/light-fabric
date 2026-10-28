use serde_json::{Value, json};
use serverless_conformance::{fixture_files, incremental, load};
use std::path::PathBuf;

fn fixture(id: &str) -> Value {
    fixture_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures"),
    )
    .unwrap()
    .iter()
    .map(|p| load(p).unwrap())
    .find(|f| f["id"] == id)
    .unwrap()
}
async fn run(id: &str) -> Value {
    let f = fixture(id);
    assert!(incremental::runner_gaps(&f).is_empty(), "{id}");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    let schema = load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/schema/result-v1.schema.json"),
    )
    .unwrap();
    serverless_conformance::schema_validate(&schema, &r).unwrap();
    assert_eq!(r["execution"], "passed", "{id}: {}", r["assertionErrors"]);
    r
}

#[tokio::test]
async fn concurrent_response_timeout_emissions_keep_independent_probe_and_dependency_counts() {
    let results =
        futures_util::future::join_all((0..4).map(|_| async {
            run("D-08e-response-timeout-emf-finalization-multi-contract-v1").await
        }))
        .await;
    for result in results {
        let observation = &result["observations"][0];
        assert_eq!(observation["boundedWorkProbeExecutions"], 1);
        assert_eq!(observation["backendCalls"].as_array().unwrap().len(), 1);
        assert_eq!(observation["emf"]["rawLines"].as_array().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn jwt_real_bootstrap_warm_verification_denial_and_token_redaction() {
    let r = run("C-03-warm-jwt-principal-not-reused-multi-contract-v1").await;
    assert_eq!(r["initialization"]["jwksFetches"], 1);
    assert_eq!(
        r["initialization"]["jwksCalls"],
        json!([{"target":"jwks","method":"GET","uri":"/jwks"}])
    );
    assert_eq!(r["observations"][0]["status"], 200);
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(r["observations"][1]["status"], 401);
    assert_eq!(r["observations"][1]["backendCalls"], json!([]));
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["event"]["headers"]["authorization"],
        "Bearer ${token:future-nbf}"
    );
    assert_eq!(r["observations"][0]["jwksFetches"], 0);
}

#[tokio::test]
async fn jwt_scope_factory_executes_with_real_owned_jwks() {
    let r = run("S-05-any-match-multi-contract-v1").await;
    assert_eq!(r["observations"][0]["status"], 200);
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn rejected_security_configuration_is_admission_only_and_records_real_fetches() {
    for id in [
        "S-14-enableMockJwt-init-contract-v1",
        "S-14-ignoreJwtExpiry-init-contract-v1",
        "S-14-logJwtToken-init-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(r["initialization"]["outcome"], "initError");
        assert_eq!(r["observations"][0]["jwksCalls"], json!([]));
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only"
        );
    }
}

#[test]
fn business_error_body_cannot_be_counted_as_ingress_admission() {
    let r = json!({"observations":[{"outcome":"response","body":json!({"code":"ERR14010"}).to_string(),"backendCalls":[{"target":"business"}]}]});
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
}

#[tokio::test]
async fn transformer_named_local_and_deny_actions_use_actual_program() {
    for id in [
        "T-01-local-native-cel-multi-contract-v1",
        "T-01-deny-native-cel-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert!(r["initialization"].is_object());
        assert_ne!(r["observations"][0]["outcome"], "initError");
    }
}

#[tokio::test]
async fn generated_correlation_and_request_time_are_closed_coupled_recipes() {
    let id = "Q-02-generated-correlation-multi-contract-v1";
    let r = run(id).await;
    let f = fixture(id);
    let obs = &r["observations"][0];
    let mut bad = obs.clone();
    bad["backendCalls"][0]["event"]["multiValueHeaders"]["x-correlation-id"] = json!(["uncoupled"]);
    assert!(incremental::assert_observation(&f["expect"], &bad).is_err());
    bad = obs.clone();
    bad["backendCalls"][0]["event"]["headers"]["x-correlation-id"] = json!("not-128-bits");
    assert!(incremental::assert_observation(&f["expect"], &bad).is_err());
    let r = run("E-10-configured-metadata-multi-contract-v1").await;
    assert_eq!(r["invocationWindows"].as_array().unwrap().len(), 1);
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["event"]["requestContext"]["requestId"],
        "test-only-request"
    );
}

#[test]
fn oauth_acquisition_cache_secondary_and_errors_use_http_wire() {
    // Production's token provider deliberately requires a process test flag for
    // HTTP loopback. Scope it to this owned runner process, never mutate the
    // test process environment while other Init tests are running.
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("results");
    let ids = [
        "B-01-loopback-acquisition-cache-secondary-multi-contract-v1",
        "B-01-malformed-oauth-error-loopback-multi-contract-v1",
        "B-01-oauth-denial-loopback-multi-contract-v1",
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_serverless-conformance"));
    command
        .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
        .args(["run", "--out"])
        .arg(&out);
    for id in ids {
        command.args(["--fixture", id]);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let r = load(&out.join(format!("{}.json", ids[0]))).unwrap();
    assert_eq!(
        r["observations"][0]["httpCalls"].as_array().unwrap().len(),
        1
    );
    assert_eq!(r["observations"][1]["httpCalls"], json!([]));
    let mut expected = fixture(ids[0])["steps"][0]["expect"].clone();
    expected["httpCalls"][0]["target"] = r["observations"][0]["httpCalls"][0]["target"].clone();
    incremental::assert_observation(&expected, &r["observations"][0]).unwrap();
    let mut java_case = r["observations"][0].clone();
    let headers = java_case["httpCalls"][0]["headers"]
        .as_object_mut()
        .unwrap();
    for (lower, upper) in [
        ("authorization", "Authorization"),
        ("content-type", "Content-Type"),
        ("accept", "Accept"),
    ] {
        let value = headers.remove(lower).unwrap();
        headers.insert(upper.into(), value);
    }
    incremental::assert_observation(&expected, &java_case).unwrap();
    let mut bad = r["observations"][0].clone();
    let header = bad["httpCalls"][0]["headers"]["authorization"][0].clone();
    bad["httpCalls"][0]["headers"]["authorization"] = json!([header.clone(), header]);
    assert!(incremental::assert_observation(&expected, &bad).is_err());

    assert_eq!(
        r["observations"][1]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for id in &ids[1..] {
        let r = load(&out.join(format!("{id}.json"))).unwrap();
        assert_eq!(
            r["observations"][0]["httpCalls"].as_array().unwrap().len(),
            1
        );
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    }
}

#[test]
fn family_declaration_reviews_require_original_contract_equality_and_digest() {
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v3.json"
    ))
    .unwrap();
    assert_eq!(metadata["reviews"].as_array().unwrap().len(), 38);
    for row in metadata["reviews"].as_array().unwrap() {
        let f = fixture(row["id"].as_str().unwrap());
        serverless_conformance::declarations::review(&f)
            .unwrap()
            .unwrap();
        let mut changed = f.clone();
        changed["title"] = json!("changed contract");
        assert!(serverless_conformance::declarations::review(&changed).is_err());
    }
}

#[test]
fn bootstrap_fetch_difference_cannot_disappear_into_zero_request_fetches() {
    let obs = json!({"outcome":"response","status":200,"body":"ok","bodyBase64":false,"headers":[],"headersAbsent":[],"backendCalls":[],"jwksFetches":0});
    let mut f = json!({"nondeterministic":[],"javaOnly":false,"rustOnly":false,"differences":["jwksFetches"],"expect":obs,"expectInit":{"outcome":"initSuccess","backendCalls":[],"jwksFetches":0},"expectJavaInit":{"outcome":"initSuccess","backendCalls":[],"jwksFetches":1}});
    let j = json!({"execution":"passed","observations":[obs],"initialization":f["expectJavaInit"]});
    let r = json!({"execution":"passed","observations":[obs],"initialization":f["expectInit"]});
    incremental::compare(&f, &j, &r).unwrap();
    f["differences"] = json!([]);
    assert!(incremental::compare(&f, &j, &r).is_err());
    f["differences"] = json!(["jwksFetches"]);
    let mut bad = j.clone();
    bad["initialization"]["jwksFetches"] = json!(2);
    assert!(incremental::compare(&f, &bad, &r).is_err());
}

#[tokio::test]
async fn secondary_expiry_matches_native_status_catalogue_through_real_service() {
    let r = run("S-06-secondary-invalid-0-dual-token-multi-contract-v1").await;
    assert_eq!(r["observations"][0]["status"], 401);
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    let body: Value = serde_json::from_str(r["observations"][0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["code"], "ERR10004");
    assert_eq!(body["description"], "Jwt token in X-Scope-Token expired");
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
}

#[tokio::test]
async fn router_transport_refusal_executes_service_without_dispatch() {
    // The original activation conflict remains in preserved evidence. Exercise
    // transport behavior only with the separately approved activated fixture.
    let f = fixture("H-04-transport-refused-enabled-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["observations"][0]["status"], 400);
    assert_eq!(r["observations"][0]["httpCalls"], json!([]));
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
    let mut f = fixture("H-04-transport-refused-multi-contract-v1");
    f["dependencies"]["http"]["responses"] = json!([{}]);
    assert!(!incremental::runner_gaps(&f).is_empty());
}

#[tokio::test]
async fn caller_target_recipe_uses_real_sdk_and_configured_identity() {
    let r = run("D-07-caller-cannot-select-target-multi-contract-v1").await;
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["target"],
        "FnLiteral"
    );
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
    let mut f = fixture("D-07-caller-cannot-select-target-multi-contract-v1");
    f["context"]["unknown"] = json!(true);
    assert!(incremental::runner_gaps(&f).contains(&"unsupported Lambda context recipe".to_owned()));
}

#[test]
fn bootstrap_metadata_addition_requires_original_typed_contracts() {
    for name in ["enableMockJwt", "ignoreJwtExpiry", "logJwtToken"] {
        let mut f = fixture(&format!("S-14-{name}-init-contract-v1"));
        let row = serverless_conformance::declarations::review(&f)
            .unwrap()
            .unwrap();
        assert_eq!(row["add"], json!(["jwksFetches"]));
        assert_eq!(row["remove"], json!([]));
        f["expectJavaInit"]["jwksFetches"] = json!(0);
        assert!(serverless_conformance::declarations::review(&f).is_err());
    }
}

#[tokio::test]
async fn unified_jwt_only_identity_allows_client_and_user_limiter_service() {
    for id in [
        "L-02-verified-client-counter-multi-contract-v1",
        "L-02-verified-user-counter-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(r["initialization"]["outcome"], "initSuccess");
        assert_eq!(r["observations"][0]["status"], 200);
        assert_eq!(
            r["observations"][0]["backendCalls"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(r["observations"][1]["status"], 429);
        assert_eq!(r["observations"][1]["backendCalls"], json!([]));
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
    }
}

#[tokio::test]
async fn unified_identity_capability_never_waives_weaker_authentication_paths() {
    for mutation in [
        "anonymous",
        "disabled",
        "alternate-prefix",
        "jwt-disabled",
        "jwt-skip",
    ] {
        let mut f = fixture("L-02-verified-client-counter-multi-contract-v1");
        match mutation {
            "anonymous" => {
                f["config"]["files"]["unified-security"]["anonymousPrefixes"] = json!(["/x"])
            }
            "disabled" => f["config"]["files"]["unified-security"]["enabled"] = json!(false),
            "alternate-prefix" => f["config"]["files"]["unified-security"]["pathPrefixAuths"]
                .as_array_mut()
                .unwrap()
                .push(json!({"prefix":"/other", "jwt":false})),
            "jwt-disabled" => f["config"]["files"]["security"]["enableVerifyJwt"] = json!(false),
            "jwt-skip" => f["config"]["files"]["security"]["skipPathPrefixes"] = json!(["/x"]),
            _ => unreachable!(),
        }
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(
            r["observations"][0]["outcome"], "initError",
            "{mutation}: {r}"
        );
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    }
}

#[test]
fn repaired_fixture_metadata_removals_require_original_contract_equality() {
    for id in [
        "L-02-verified-client-counter-multi-contract-v1",
        "L-02-verified-user-counter-multi-contract-v1",
        "S-06-secondary-invalid-0-dual-token-multi-contract-v1",
    ] {
        let f = fixture(id);
        let row = serverless_conformance::declarations::review(&f)
            .unwrap()
            .unwrap();
        assert!(row["remove"].as_array().unwrap().len() >= 2);
        assert!(row.get("add").is_none());
        let mut changed = f;
        changed["title"] = json!("changed source contract");
        assert!(serverless_conformance::declarations::review(&changed).is_err());
    }
}

#[test]
fn default_amendment_comparison_applies_bootstrap_additions_without_changing_expectations() {
    for name in ["enableMockJwt", "ignoreJwtExpiry", "logJwtToken"] {
        let f = fixture(&format!("S-14-{name}-init-contract-v1"));
        let mut java = f["expectJava"].clone();
        for call in java["backendCalls"].as_array_mut().unwrap() {
            for (key, value) in call["eventExact"].as_object().unwrap().clone() {
                call["event"][key] = value;
            }
        }
        let j = json!({"execution":"passed", "observations":[java], "initialization":f["expectJavaInit"]});
        let r = json!({"execution":"passed", "observations":[f["expect"]], "initialization":f["expectInit"]});
        serverless_conformance::amendments::compare(&f, &j, &r, false).unwrap();
        assert!(serverless_conformance::amendments::compare(&f, &j, &r, true).is_err());
        let mut bad = j;
        bad["initialization"]["jwksFetches"] = json!(2);
        assert!(serverless_conformance::amendments::compare(&f, &bad, &r, false).is_err());
    }
}

#[test]
fn refused_http_body_recipe_binds_only_loopback_origin_and_preserves_exact_bytes() {
    let f = fixture("H-04-loopback-test-refused-represented-v1");
    let mut java = f["expectJava"].clone();
    java["body"] = json!(
        java["body"]
            .as_str()
            .unwrap()
            .replace("${dependency:http}", "http://127.0.0.1:43210")
    );
    let j = json!({"execution":"passed","observations":[java]});
    let r = json!({"execution":"passed","observations":[f["expect"]]});
    serverless_conformance::amendments::compare(&f, &j, &r, false).unwrap();
    for replacement in [
        "http://example.com:43210",
        "http://127.0.0.1:43210/extra",
        "https://127.0.0.1:43210",
    ] {
        let mut bad = j.clone();
        bad["observations"][0]["body"] = json!(
            f["expectJava"]["body"]
                .as_str()
                .unwrap()
                .replace("${dependency:http}", replacement)
        );
        assert!(serverless_conformance::amendments::compare(&f, &bad, &r, false).is_err());
    }
    let mut bad = j;
    bad["observations"][0]["body"] = json!(
        bad["observations"][0]["body"]
            .as_str()
            .unwrap()
            .replace("Failed to invoke", "Failed  to invoke")
    );
    assert!(serverless_conformance::amendments::compare(&f, &bad, &r, false).is_err());
}

#[tokio::test]
async fn forged_secondary_expiry_diagnostic_cannot_trigger_second_refresh() {
    let r = run("S-06-secondary-forged-read-claims-multi-contract-v1").await;
    assert_eq!(r["initialization"]["jwksFetches"], 1);
    assert_eq!(r["observations"][0]["jwksFetches"], 1);
    assert_eq!(r["observations"][0]["status"], 401);
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    let body: Value = serde_json::from_str(r["observations"][0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["code"], "ERR10003");
}

#[tokio::test]
async fn approved_wrong_issuer_and_provider_order_amendments_preserve_originals() {
    for id in [
        "S-04a-wrong-issuer-multi-contract-v1",
        "S-13-normalized-service-set-cooldown-multi-contract-v1",
    ] {
        let f = fixture(id);
        let row = serverless_conformance::amendments::review(&f)
            .unwrap()
            .unwrap();
        assert_eq!(row["status"], "approved-active-v2");
        let r = run(id).await;
        if id.starts_with("S-04a") {
            let body: Value =
                serde_json::from_str(r["observations"][0]["body"].as_str().unwrap()).unwrap();
            assert_eq!(body["code"], "ERR10000");
            assert_eq!(body["message"], "INVALID_AUTH_TOKEN");
            assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        } else {
            assert_eq!(r["observations"][0]["jwksCalls"][0]["uri"], "/other");
            assert_eq!(r["observations"][0]["jwksCalls"][1]["uri"], "/jwks");
            assert_eq!(r["observations"][1]["jwksFetches"], 0);
        }
        let original = incremental::run_fixture_original(&f, &"0".repeat(40))
            .await
            .unwrap();
        assert_eq!(original["execution"], "failed");
        let mut changed = f;
        changed["title"] = json!("changed original");
        assert!(serverless_conformance::amendments::review(&changed).is_err());
    }
}

#[tokio::test]
async fn explicitly_activated_router_refusal_runs_real_service() {
    let r = run("H-04-transport-refused-enabled-contract-v1").await;
    assert_eq!(r["observations"][0]["status"], 400);
    assert_eq!(r["observations"][0]["httpCalls"], json!([]));
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
}

#[test]
fn refused_dependency_origin_projection_keeps_non_origin_body_changes_exact() {
    let f = fixture("H-04-transport-refused-enabled-contract-v1");
    let make = |side: &str, port: u16| {
        let mut obs = f[side].clone();
        obs["body"] = json!(
            obs["body"]
                .as_str()
                .unwrap()
                .replace("${dependency:http}", &format!("http://127.0.0.1:{port}"))
        );
        json!({"execution":"passed", "observations":[obs]})
    };
    let j = make("expect", 41001);
    let r = make("expect", 41002);
    serverless_conformance::amendments::compare(&f, &j, &r, false).unwrap();
    let mut bad = r;
    bad["observations"][0]["body"] = json!(
        bad["observations"][0]["body"]
            .as_str()
            .unwrap()
            .replace("Failed to invoke", "Failed  to invoke")
    );
    assert!(serverless_conformance::amendments::compare(&f, &j, &bad, false).is_err());
}

#[tokio::test]
async fn verified_authorizer_adapter_compares_exact_signing_inputs_not_forged_ingress() {
    let id = "E-08-verified-authorizer-multi-contract-v1";
    let f = fixture(id);
    let r = run(id).await;
    let a = &r["observations"][0]["backendCalls"][0]["event"]["requestContext"]["authorizer"];
    assert_eq!(a["principalId"], "test-user");
    assert!(a.get("unapproved").is_none());
    assert_eq!(a["claims"]["sub"], "test-user");
    assert_eq!(
        r["tokenSigningEpochs"][0]["epochs"]["valid"]
            .as_i64()
            .unwrap()
            - 10,
        a["claims"]["iat"].as_i64().unwrap()
    );
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
    // Compare the same Rust contract on both sides only as an assertion negative
    // control; the real differential test uses separately recorded native output.
    let mut same = f.clone();
    same.as_object_mut().unwrap().remove("expectJava");
    same["differences"] = json!([]);
    incremental::compare(&same, &r, &r).unwrap();
    for field in ["principalId", "unapproved", "claims"] {
        let mut bad = r.clone();
        let a =
            &mut bad["observations"][0]["backendCalls"][0]["event"]["requestContext"]["authorizer"];
        match field {
            "claims" => a["claims"]["exp"] = json!(0),
            _ => a[field] = json!("forged"),
        }
        assert!(incremental::compare(&same, &r, &bad).is_err());
    }
    let mut missing = r.clone();
    missing
        .as_object_mut()
        .unwrap()
        .remove("tokenSigningEpochs");
    assert!(incremental::compare(&same, &r, &missing).is_err());
}

#[tokio::test]
async fn legacy_init_rejection_records_each_unexecuted_step_without_service() {
    for id in [
        "S-12-disabled-selected-basic-multi-contract-v1",
        "B-01-no-service-represented-v1",
    ] {
        let f = fixture(id);
        assert!(
            incremental::runner_gaps(&f).is_empty(),
            "{:?}",
            incremental::runner_gaps(&f)
        );
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "passed", "{r}");
        assert_eq!(r["initialization"]["outcome"], "initError");
        assert_eq!(
            r["observations"].as_array().unwrap().len(),
            f["steps"].as_array().unwrap().len()
        );
        for o in r["observations"].as_array().unwrap() {
            assert_eq!(o["outcome"], "notExecuted");
            assert_eq!(o["backendCalls"], json!([]));
            let mut bad = o.clone();
            bad["reason"] = json!("made-up");
            assert!(incremental::assert_observation(&f["steps"][0]["expect"], &bad).is_err());
        }
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only"
        );
    }
}

#[tokio::test]
async fn init_only_runs_real_admission_without_generating_an_alb_request() {
    let r = run("D-09-response-cap-minimum-4095-init-contract-v1").await;
    assert_eq!(r["observations"][0]["outcome"], "initError");
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "admission-only"
    );
}

#[test]
fn archived_java_exception_assertions_are_exact() {
    let e = fixture("D-06-nonobject-0-v1")["expectJava"].clone();
    incremental::assert_observation(&e, &e).unwrap();
    for field in ["class", "phase", "outcome", "backendCalls"] {
        let mut bad = e.clone();
        bad[field] = json!("different");
        assert!(
            incremental::assert_observation(&e, &bad).is_err(),
            "{field}"
        );
    }
}

#[tokio::test]
async fn sdk_client_transport_reset_is_classified_by_real_sdk() {
    let f = fixture("D-06-typed-sdk-0-multi-contract-v1");
    let r = incremental::run_fixture_original(&f, &"0".repeat(40))
        .await
        .unwrap();
    // Preserve the literal-target expectation. Production intentionally exposes
    // a logical safe target; report that contract conflict rather than waive it.
    assert_eq!(r["execution"], "failed");
    assert_eq!(r["observations"][0]["status"], 502);
    assert!(
        r["observations"][0]["body"]
            .as_str()
            .unwrap()
            .contains("function0")
    );
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "service-execution"
    );
    let active = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(active["execution"], "passed");
    let r = run("D-01-typed-sdk-0-v1").await;
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "admission-only"
    );
}

#[tokio::test]
async fn runtime_identity_self_target_is_rejected_by_real_init() {
    let r = run("D-07-self-name-init-contract-v1").await;
    assert_eq!(r["observations"][0]["outcome"], "initError");
    let mut f = fixture("D-07-self-name-init-contract-v1");
    f["runtimeIdentity"]["accountId"] = json!("unapproved");
    assert!(!incremental::runner_gaps(&f).is_empty());
}

#[tokio::test]
async fn audit_observation_uses_production_sink_and_exact_record_bytes() {
    for id in [
        "A-01-safe-selected-stdout-multi-contract-v1",
        "A-02-selected-string-ascii-at-1024-contract-v1",
        "A-02-selected-string-ascii-over-1024-contract-v1",
        "A-02-selected-string-utf8-at-1024-contract-v1",
        "A-02-selected-string-utf8-over-1024-contract-v1",
    ] {
        let f = fixture(id);
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "passed", "{r}");
        let o = &r["observations"][0];
        assert_eq!(o["backendCalls"].as_array().unwrap().len(), 1);
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
        let f = fixture(id);
        let mut bad = o.clone();
        bad["auditRecords"] = json!([]);
        assert!(incremental::assert_observation(&f["steps"][0]["expect"], &bad).is_err());
        let mut bad = o.clone();
        bad["auditRecords"][0]["data"]["unapproved"] = json!(true);
        assert!(incremental::assert_observation(&f["steps"][0]["expect"], &bad).is_err());
    }
}

#[tokio::test]
async fn archived_generated_scalar_correlation_checks_native_shape_and_real_rust_admission() {
    for id in [
        "B-05-null-response-headers-cp1-e02-accepted",
        "Q-02-generated-null-map-cp1-e02-accepted",
    ] {
        let f = fixture(id);
        let r = run(id).await;
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only"
        );
        let mut java = f["expectJava"].clone();
        java["backendCalls"][0]["event"]["headers"]["X-Correlation-Id"] = json!("bad");
        let j = json!({"execution":"passed","observations":[java]});
        assert!(
            incremental::compare(&f, &j, &r)
                .unwrap_err()
                .to_string()
                .contains("descriptor")
        );
        let mut unsupported = f;
        unsupported["nondeterministic"][0]["field"] = json!("/body");
        assert!(!incremental::runner_gaps(&unsupported).is_empty());
    }
}

#[tokio::test]
async fn explicit_insecure_config_recipe_is_process_isolated_and_expectations_remain_strict() {
    let before = std::env::var_os("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG");
    let f = fixture("S-14-enableMockJwt-explicit-test-override-contract-v1");
    assert!(incremental::runner_gaps(&f).is_empty());
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(
        std::env::var_os("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG"),
        before
    );
    assert_eq!(r["initialization"]["outcome"], "initSuccess");
    // The preserved expectation says /x; the actual configured request is
    // /pets/fixed. Expose the contract conflict instead of amending it here.
    assert_eq!(r["execution"], "passed");
    assert_eq!(r["initialization"]["jwksFetches"], 0);
    let original = incremental::run_fixture_original(&f, &"0".repeat(40))
        .await
        .unwrap();
    assert_eq!(original["execution"], "failed");
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["event"]["path"],
        "/pets/fixed"
    );
    let mut bad = f;
    bad["insecureTestConfigPrerequisite"] = json!("other");
    assert!(!incremental::runner_gaps(&bad).is_empty());
}

#[tokio::test]
async fn a_legacy_init_declaration_cannot_force_or_fabricate_rejection() {
    let mut f = fixture("E-03-response-off-0-multi-v1");
    f["expectRustInit"] = json!({"outcome":"initError","code":"InvalidConfiguration","backendCalls":[],"jwksFetches":0});
    f["expect"] =
        json!({"outcome":"notExecuted","reason":"initError","backendCalls":[],"jwksFetches":0});
    let r = incremental::run_fixture_original(&f, &"0".repeat(40))
        .await
        .unwrap();
    assert_eq!(r["execution"], "failed");
    assert_eq!(r["initialization"]["outcome"], "initSuccess");
    assert_eq!(r["observations"][0]["outcome"], "response");
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn admission_declaration_removals_reprove_unchanged_original_contracts() {
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v6.json"
    ))
    .unwrap();
    assert_eq!(metadata["reviews"].as_array().unwrap().len(), 42);
    for row in metadata["reviews"].as_array().unwrap() {
        let f = fixture(row["id"].as_str().unwrap());
        serverless_conformance::declarations::review(&f)
            .unwrap()
            .unwrap();
        let mut changed = f;
        changed["title"] = json!("changed");
        assert!(serverless_conformance::declarations::review(&changed).is_err());
    }
}

#[tokio::test]
async fn approved_v3_contracts_use_real_service_and_preserve_originals() {
    for id in [
        "D-06-typed-sdk-0-multi-contract-v1",
        "S-14-enableMockJwt-explicit-test-override-contract-v1",
        "S-14-ignoreJwtExpiry-explicit-test-override-contract-v1",
        "S-14-logJwtToken-explicit-test-override-contract-v1",
    ] {
        let f = fixture(id);
        let row = serverless_conformance::amendments::review(&f)
            .unwrap()
            .unwrap();
        assert_eq!(row["status"], "approved-active-v3");
        let r = run(id).await;
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
        if id.contains("ignoreJwtExpiry") || id.contains("logJwtToken") {
            assert_eq!(r["observations"][0]["status"], 401);
            assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        }
        let mut changed = f;
        changed["title"] = json!("changed original");
        assert!(serverless_conformance::amendments::review(&changed).is_err());
    }
}

#[tokio::test]
async fn typed_init_warnings_are_captured_from_production() {
    for mode in ["absent", "default-false"] {
        let r = run(&format!("CFG-04-handler-duration-{mode}-init-contract-v1")).await;
        assert_eq!(
            r["observations"][0]["configurationWarningObservation"]["warningCount"],
            1
        );
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only"
        );
    }
}

#[tokio::test]
async fn detached_refresh_uses_real_caller_deadline_and_publishes_for_warm_service() {
    let r = run("S-13c-detached-refresh-cache-publish-multi-contract-v1").await;
    assert_eq!(r["observations"][0]["status"], 504);
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    assert_eq!(r["observations"][1]["status"], 200);
    assert_eq!(r["observations"][1]["jwksFetches"], 0);
    assert_eq!(
        r["observations"][1]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn jwt_privacy_observes_production_verification_and_logs() {
    for state in ["expired", "verified"] {
        let r = run(&format!(
            "S-14-{state}-jwt-no-secret-logs-multi-contract-v1"
        ))
        .await;
        assert_eq!(
            r["observations"][0]["securityLogObservation"],
            json!({
                "fullTokenExposed":false,"tokenFragmentExposed":false,"sensitiveClaimExposed":false
            })
        );
    }
}

#[test]
fn duration_assertions_require_measured_witness_and_exact_bounds() {
    let e = json!({"outcome":"response","status":200,"headers":[],"headersAbsent":[],"body":"ok","bodyBase64":false,"backendCalls":[],"invocationTiming":{"minimumMs":100,"maximumMs":200}});
    let mut a = json!({"outcome":"response","status":200,"headers":[],"body":"ok","bodyBase64":false,"backendCalls":[]});
    assert!(incremental::assert_observation(&e, &a).is_err());
    for ms in [99, 201] {
        a["invocationDurationMs"] = json!(ms);
        assert!(incremental::assert_observation(&e, &a).is_err());
    }
    for ms in [100, 200] {
        a["invocationDurationMs"] = json!(ms);
        incremental::assert_observation(&e, &a).unwrap();
    }
}

#[tokio::test]
async fn bounded_work_runs_inside_actual_init_compiled_pipeline() {
    for id in [
        "D-08a-request-bounded-work-multi-contract-v1",
        "D-08b-response-bounded-work-multi-contract-v1",
        "D-08-absolute-cross-stage-budget-multi-contract-v1",
        "D-08-ingress-hard-limit-minimum-multi-contract-v1",
    ] {
        let r = run(id).await;
        let a = &r["observations"][0];
        assert_eq!(a["status"], 504);
        assert_eq!(
            a["boundedWorkProbeExecutions"],
            if id.contains("absolute") { 2 } else { 1 }
        );
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
    }
}

#[tokio::test]
async fn response_policy_counts_decorate_real_header_handler_and_finalizer() {
    for id in [
        "D-09-response-policy-headers-limit-counter-multi-contract-v1",
        "D-09-minimum-cap-replacement-recheck-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(
            r["observations"][0]["responsePolicyObservation"],
            json!({"handler":"response0","entries":1,"completions":1,"finalizations":1})
        );
    }
}

#[tokio::test]
async fn request_failure_probes_reach_actual_pipeline_guard() {
    for mode in ["return-null-status", "throw-test-exception"] {
        let r = run(&format!("C-04-{mode}-multi-contract-v1")).await;
        assert_eq!(r["observations"][0]["status"], 500);
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        assert_eq!(
            r["observations"][0]["failureProbeResult"],
            json!({"mode":mode,"executions":1})
        );
    }
}

#[tokio::test]
async fn actual_bootstrap_reports_init_rejection_and_exits_before_service() {
    for id in [
        "CFG-05-missing-stage-init-report-contract-v1",
        "D-07-self-name-alias-init-contract-v1",
        "D-07-self-name-version-init-contract-v1",
        "D-07-self-arn-alias-init-contract-v1",
        "D-07-self-arn-version-init-contract-v1",
        "S-14-test-override-does-not-relax-max-skew-init-contract-v1",
    ] {
        let r = run(id).await;
        let report = &r["observations"][0]["initFailureReporting"];
        assert_eq!(report["exitCode"], 1);
        assert_eq!(report["reportCount"], 1);
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only"
        );
    }
}

#[tokio::test]
async fn emf_captures_actual_stdout_after_real_service_completion() {
    for id in [
        "M-01-per-request-emf-multi-contract-v1",
        "M-01-gateway-error-emf-multi-contract-v1",
        "M-01-oversize-emf-completion-multi-contract-v1",
        "M-01-request-timeout-emf-completion-multi-contract-v1",
        "D-08e-response-timeout-emf-finalization-multi-contract-v1",
        "D-08e-response-timeout-emf-finalization-counter-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(
            r["observations"][0]["emf"]["rawLines"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
        let f = fixture(id);
        let mut missing = r["observations"][0].clone();
        missing["emf"]["rawLines"] = json!([]);
        assert!(incremental::assert_observation(&f["expect"], &missing).is_err());
        let mut extra = r["observations"][0].clone();
        let line = extra["emf"]["rawLines"][0].clone();
        extra["emf"]["rawLines"].as_array_mut().unwrap().push(line);
        assert!(incremental::assert_observation(&f["expect"], &extra).is_err());
    }
}

#[tokio::test]
async fn warm_single_value_diagnostic_observes_once_per_actual_process() {
    let r = run("E-13-warm-single-value-config-log-once-contract-v1").await;
    assert_eq!(
        r["observations"][0]["configurationDiagnostics"]["multiValueRequiredLogs"],
        1
    );
    assert_eq!(
        r["observations"][1]["configurationDiagnostics"]["multiValueRequiredLogs"],
        0
    );
    assert_eq!(
        r["configurationDiagnosticEvidence"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        serverless_conformance::amendments::coverage(&r),
        "admission-only"
    );
}

#[tokio::test]
async fn function_error_adapter_exposes_preserved_contract_conflicts() {
    for id in [
        "D-03-handled-multi-contract-v1",
        "D-03-unhandled-multi-contract-v1",
        "D-03-D-04-safe-metadata-no-log-tail-multi-contract-v1",
    ] {
        let f = fixture(id);
        assert!(incremental::runner_gaps(&f).is_empty());
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        let schema = load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/serverless-conformance/schema/result-v1.schema.json"),
        )
        .unwrap();
        serverless_conformance::schema_validate(&schema, &r).unwrap();
        assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
        let a = &r["observations"][0];
        assert_eq!(a["status"], 502);
        assert_eq!(a["backendCalls"].as_array().unwrap().len(), 1);
        let metadata: Value =
            serde_json::from_str(a["backendDiagnostics"][0]["message"].as_str().unwrap()).unwrap();
        assert_eq!(metadata["targetAlias"], "function0");
        assert_eq!(metadata["executedVersion"], Value::Null);
        assert_eq!(
            f["expect"]["backendDiagnostics"]["metadata"]["executedVersion"],
            Value::Null
        );
        assert!(incremental::assert_observation(&f["expect"], a).is_err());
        assert_eq!(
            f["expect"]["backendDiagnostics"]["metadata"]["targetAlias"],
            "FnX"
        );
        assert_eq!(a["backendCalls"][0]["target"], "FnX");
        assert_eq!(a["backendCalls"][0]["invoke"]["logType"], "None");
    }
}

#[tokio::test]
async fn root_key_publication_is_applied_after_init_and_warm_identity_is_not_ignored() {
    let mut f = fixture("S-13c-detached-refresh-cache-publish-multi-contract-v1");
    f["jwksPublishedKeys"] = json!(["k2"]);
    for step in f["steps"].as_array_mut().unwrap() {
        step.as_object_mut().unwrap().remove("jwksPublishedKeys");
    }
    assert!(incremental::runner_gaps(&f).is_empty());
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["initialization"]["jwksFetches"], 1);
    assert_eq!(r["observations"][0]["status"], 504);
    assert_eq!(r["observations"][1]["status"], 200);
    assert_eq!(r["observations"][1]["jwksFetches"], 0);
    f["steps"][0]["context"]["functionName"] = json!("different-warm-identity");
    assert!(
        incremental::runner_gaps(&f)
            .contains(&"warm Lambda identity cannot change after Init".to_owned())
    );
}

#[tokio::test]
async fn parallel_refresh_runs_two_real_callers_through_one_shared_rust_fetch() {
    let r = run("S-13b-active-refresh-join-multi-contract-v1").await;
    assert_eq!(
        r["observations"][0]["responses"].as_array().unwrap().len(),
        2
    );
    assert_eq!(r["observations"][0]["jwksFetches"], 1);
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    for response in r["observations"][0]["responses"].as_array().unwrap() {
        assert_eq!(response["status"], 401);
    }
}
#[tokio::test]
async fn sdk_budget_zero_prevents_attempt_but_valid_budget_reaches_actual_sdk() {
    let original = fixture("D-08-zero-remaining-multi-contract-v1");
    let rejected = incremental::run_fixture(&original, &"0".repeat(40))
        .await
        .unwrap();
    assert_eq!(rejected["execution"], "failed");
    assert_eq!(rejected["observations"][0]["outcome"], "initError");
    let mut f = original.clone();
    f["config"]["files"]["lambda-proxy"]
        .as_object_mut()
        .unwrap()
        .remove("maxRetryAttempts");
    let zero = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(zero["execution"], "passed", "{}", zero["assertionErrors"]);
    assert_eq!(zero["observations"][0]["status"], 504);
    assert_eq!(zero["observations"][0]["backendCalls"], json!([]));
    f["context"]["remainingTimeMs"] = json!(3000);
    for response in f["dependencies"]["invoke"]["responses"]
        .as_array_mut()
        .unwrap()
    {
        response["delayMs"] = json!(0);
    }
    f["expect"] = json!({"outcome":"response","status":201,"headers":[["x-sdk","loopback"]],"headersAbsent":[],"body":"sdk-ok","bodyBase64":false,"jwksFetches":0,"backendCalls":[{"target":"test-only-sdk","event":{},"invoke":{"logType":"None"},"sdkHttp":{"method":"POST","uri":"/2015-03-31/functions/test-only-sdk/invocations","endpoint":"${dependency:invoke}"}}]});
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn transformer_error_observation_delegates_to_real_engine_and_retains_input_difference() {
    for id in [
        "T-01-caught-native-cel-rule-error-multi-contract-v1",
        "T-02-caught-native-cel-rule-error-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(r["observations"][0]["ruleFailuresCaught"], 1);
        assert_eq!(
            r["observations"][0]["backendCalls"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    let f = fixture("T-02-actual-native-response-input-map-multi-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    assert_eq!(
        r["observations"][0]["ruleInputObservation"]["input"]["queryParameters"],
        Value::Null
    );
    assert_eq!(r["observations"][0]["body"], "response-transformed");
}

#[tokio::test]
async fn final_adapters_keep_configuration_coverage_separate_from_execution() {
    for id in [
        "CFG-02-loopback-jks-tls-represented-v1",
        "CFG-02-sdk-netty-trust-boundary-represented-v1",
        "S-16-loopback-bind-denial-represented-v1",
        "S-17-controlled-cached-nbf-mutation-v1",
    ] {
        let r = run(id).await;
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "admission-only",
            "{id}"
        );
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    }
    for id in [
        "D-08c-blocking-response-lease-multi-contract-v1",
        "D-08c-blocking-capacity-two-callers-multi-contract-v1",
    ] {
        let r = run(id).await;
        assert_eq!(
            serverless_conformance::amendments::coverage(&r),
            "service-execution"
        );
    }
}
#[tokio::test]
async fn completion_callbacks_really_execute_and_original_phase_order_remains_explicit() {
    let f = fixture("C-03-completion-probe-multi-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    let actual = &r["observations"][0]["completionProbeResult"];
    assert_eq!(
        actual["order"],
        json!([
            "response-last",
            "response-first",
            "request-last",
            "request-first"
        ])
    );
    assert_eq!(actual["countBeforeRepeat"], 4);
    assert_eq!(actual["countAfterRepeat"], 4);
    assert_eq!(actual["requestRepeatRejected"], true);
    assert_eq!(actual["responseRepeatRejected"], true);
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn trace_context_observes_dispatch_without_inventing_logging_context() {
    let f = fixture("Q-01a-warm-trace-context-multi-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(
        r["observations"][0]["traceContext"]["exchangeTraceabilityId"],
        "test-only-trace"
    );
    assert_eq!(
        r["observations"][0]["traceContext"]["logContextTraceabilityId"],
        "test-only-trace"
    );
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        r["observations"][1]["traceContext"]["logContextTraceabilityId"],
        Value::Null
    );
}

#[tokio::test]
async fn final_single_runner_components_use_contract_skips_and_real_probes() {
    for id in [
        "C-07-stream-init-config-type-component-v1",
        "P-01a-java-bootstrap-component-v1",
        "K-01-loopback-create-remove-component-v1",
        "K-01-loopback-crud-component-v1",
    ] {
        let f = fixture(id);
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "skipped");
        assert_eq!(r["observations"], json!([]));
    }
    for id in [
        "CFG-01-configured-debug-v1-contract-v1",
        "CFG-01-configured-error-v1-contract-v1",
        "CFG-01-default-info-v1-contract-v1",
        "CFG-05-runtime-api-unset-v1-contract-v1",
        "CFG-05-loopback-connection-refused-v1-contract-v1",
        "CFG-05-loopback-http-503-v1-contract-v1",
        "CFG-05-loopback-response-withheld-v1-contract-v1",
    ] {
        run(id).await;
    }
}

#[tokio::test]
async fn native_gcm_and_selected_dynamo_observations_remain_real_failures() {
    for id in [
        "CFG-03-synthetic-decryption-init-probe-v1",
        "K-01-selected-dynamo-deferred-init-contract-v1",
    ] {
        let f = fixture(id);
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "passed", "{id}: {}", r["assertionErrors"]);
        assert_eq!(
            r["observations"][0]["outcome"],
            if id.starts_with("CFG") {
                "initSuccess"
            } else {
                "initError"
            }
        );
        assert_eq!(r["observations"][0]["backendCalls"], json!([]));
        if id.starts_with("CFG") {
            assert_eq!(
                r["observations"][0]["configurationObservation"]["decrypted"],
                "test-only-decrypted-value"
            );
        }
    }
}
#[tokio::test]
async fn actual_gate_components_keep_cancellation_code_and_scoped_cache_differences() {
    for id in [
        "S-13a-cancelled-shared-fetch-component-v1",
        "S-13c-shared-fetch-panic-recovery-component-v1",
    ] {
        let f = fixture(id);
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "passed", "{id}");
        let schema = load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/serverless-conformance/schema/result-v1.schema.json"),
        )
        .unwrap();
        serverless_conformance::schema_validate(&schema, &r).unwrap();
        if id.starts_with("S-13a") {
            assert_eq!(
                r["observations"][0]["gate"]["followupUnknownKid"]["code"],
                "ERR10002"
            );
            assert_eq!(
                r["observations"][0]["gate"]["followupUnknownKid"]["jwksFetches"],
                0
            );
        } else {
            assert_eq!(r["observations"][0]["failure"]["waitersTerminated"], 3);
            assert_eq!(r["observations"][0]["failure"]["networkRequests"], 0);
            assert_eq!(r["observations"][0]["recovery"]["verifiedPrincipals"], 1);
            assert_eq!(r["observations"][0]["recovery"]["cachePublished"], false);
        }
    }
}
#[tokio::test]
async fn real_preparation_panic_preserves_redacted_message_and_actual_wire_header() {
    let f = fixture("CFG-05-prepare-local-config-panic-init-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    let report = &r["observations"][0]["initFailureReporting"];
    assert_eq!(
        report["header"]["Lambda-Runtime-Function-Error-Type"],
        "Runtime.InitPanic"
    );
    assert_eq!(report["body"]["errorMessage"], "initialization: panic");
    assert_eq!(report["reportCount"], 1);
    assert_eq!(report["invocationLoopEntered"], false);
}
