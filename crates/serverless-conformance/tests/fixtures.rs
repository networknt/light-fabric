use serde_json::{Value, json};
use serverless_conformance::{
    fixture_files,
    incremental::{self, SUPPORTED},
    load,
};
use std::path::PathBuf;

fn fixture(id: &str) -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1");
    fixture_files(&root)
        .unwrap()
        .iter()
        .map(|p| load(p).unwrap())
        .find(|f| f["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn admitted_native_fixtures_through_actual_app_init_and_service() {
    for id in SUPPORTED {
        let f = fixture(id);
        let r = incremental::run_fixture_original(&f, &"0".repeat(40))
            .await
            .unwrap();
        if *id == "S-08-missing-basic-multi-contract-v1" {
            assert_eq!(r["execution"], "failed");
            assert_eq!(
                r["assertionErrors"],
                json!(["step 0: response header multimap mismatch"])
            );
            continue;
        }
        assert_eq!(r["execution"], "passed", "{id}: {}", r["assertionErrors"]);
        let mut equivalent = f.clone();
        equivalent.as_object_mut().unwrap().remove("expectJava");
        equivalent.as_object_mut().unwrap().remove("differences");
        incremental::compare(&equivalent, &r, &r).unwrap();
        // This is a comparator unit oracle, not recorded Java execution evidence.
        let mut j = r.clone();
        j["observations"][0] = f["expectJava"].clone();
        let calls = j["observations"][0]["backendCalls"].as_array_mut().unwrap();
        for call in calls {
            if let Some(exact) = call.get("eventExact").and_then(Value::as_object).cloned() {
                for (k, v) in exact {
                    call["event"][k] = v;
                }
            }
            call.as_object_mut().unwrap().remove("eventExact");
            call.as_object_mut().unwrap().remove("eventAbsent");
        }
        if [
            "V-03-get-body-bypass-multi-contract-v1",
            "V-03-html-content-encoder-multi-contract-v1",
            "V-03-json-object-multi-contract-v1",
            "V-03-malformed-json-multi-contract-v1",
            "V-03-xml-content-encoder-multi-contract-v1",
        ]
        .contains(id)
        {
            assert!(
                incremental::compare(&f, &j, &r)
                    .unwrap_err()
                    .to_string()
                    .contains("stale")
            );
        } else {
            incremental::compare(&f, &j, &r).unwrap();
        }
        let mut bad = r.clone();
        bad["observations"][0]["body"] = json!("changed body bytes");
        assert!(incremental::compare(&f, &j, &bad).is_err());
        let mut stale = f.clone();
        stale["differences"] = json!([]);
        if *id == "V-03-malformed-json-multi-contract-v1" {
            // Both actual runners already agree here; the preserved original
            // declaration is stale. This unit copy is not a fixture amendment.
            incremental::compare(&stale, &j, &r).unwrap();
        } else {
            assert!(incremental::compare(&stale, &j, &r).is_err());
        }
        let mut stale = f.clone();
        stale["differences"] = json!(["backendCalls", "status"]);
        assert!(incremental::compare(&stale, &j, &r).is_err());
        if !r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            let mut bad = r.clone();
            bad["observations"][0]["backendCalls"][0]["target"] = json!("wrong-target");
            assert!(incremental::compare(&f, &j, &bad).is_err());
        }
    }
}

#[tokio::test]
async fn repeated_correlation_forwards_the_policy_selected_scalar_and_original_multimap() {
    let f = fixture("Q-02-repeated-correlation-multi-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(
        f["expect"]["backendCalls"][0]["eventExact"]["headers"]["x-correlation-id"],
        "test-only-first"
    );
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["event"]["headers"]["x-correlation-id"],
        "test-only-first"
    );
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["event"]["multiValueHeaders"]["x-correlation-id"],
        json!(["test-only-first", "test-only-second"])
    );
    assert_eq!(r["execution"], "passed");
    assert_eq!(r["assertionErrors"], json!([]));
}

#[test]
fn selected_missing_adapter_is_incomplete_and_never_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let args = vec![
        "run".into(),
        "--fixture".into(),
        "A-01-mask-error-parameters-sink-init-v1".into(),
        "--out".into(),
        out.to_string_lossy().into(),
    ];
    assert!(incremental::command(&args).is_err());
    let coverage = load(&out.join("coverage.json")).unwrap();
    assert_eq!(coverage["selected"], 1);
    assert_eq!(coverage["unimplemented"], 1);
    assert_eq!(coverage["pass"], 0);
    assert_eq!(coverage["complete"], false);
    assert_eq!(coverage["runnerUnsupported"], 1);
    let row = coverage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["selected"] == true)
        .unwrap();
    assert_eq!(row["runnerSupport"], "missing");
    assert!(
        row["handlerSupport"]["registered"]
            .as_array()
            .unwrap()
            .contains(&json!("proxy"))
    );
    assert_eq!(row["handlerSupport"]["unregistered"], json!([]));
    assert!(
        !out.join("A-01-mask-error-parameters-sink-init-v1.json")
            .exists()
    );
    assert!(
        incremental::command(&args).is_err(),
        "output must not overwrite prior evidence"
    );
}

#[test]
fn runner_gaps_are_independent_of_registered_missing_and_deferred_handlers() {
    let classes = serverless_core::compat::COMPATIBILITY;
    let declarations = [
        "sanitizer",
        "audit",
        "requestTransformer",
        "router",
        "validator",
        "swt",
    ]
    .map(|alias| {
        format!(
            "{}@{alias}",
            classes.iter().find(|c| c.alias == alias).unwrap().class
        )
    });
    let support = incremental::handler_support(
        &json!({"config":{"files":{"handler":{"handlers":declarations}}}}),
    );
    for alias in ["sanitizer", "audit", "requestTransformer", "router"] {
        assert!(
            support["registered"]
                .as_array()
                .unwrap()
                .contains(&json!(alias))
        );
    }
    assert_eq!(support["unregistered"], json!(["validator"]));
    assert_eq!(support["deferred"], json!(["swt"]));
}

#[test]
fn strict_header_multimap_and_absence_null_empty_assertions() {
    let e = json!({"outcome":"response","status":200,"body":null,"bodyBase64":false,"headers":[["X-Cookie","a"],["x-cookie","b"]],"headersAbsent":["secret"],"jwksFetches":0,"backendCalls":[{"target":"Fn","event":{"body":null},"eventAbsent":["/missing"],"eventExact":{"headers":null}}]});
    let a = json!({"outcome":"response","status":200,"body":null,"bodyBase64":false,"headers":[["x-cookie","a"],["X-COOKIE","b"]],"jwksFetches":0,"backendCalls":[{"target":"Fn","event":{"body":null,"headers":null}}]});
    incremental::assert_observation(&e, &a).unwrap();
    for pointer in [
        "/body",
        "/backendCalls/0/event/body",
        "/backendCalls/0/event/headers",
    ] {
        let mut bad = a.clone();
        *bad.pointer_mut(pointer).unwrap() = json!("");
        assert!(incremental::assert_observation(&e, &bad).is_err());
    }
    let mut bad = a.clone();
    bad["headers"] = json!([["X-Cookie", "b"], ["x-cookie", "a"]]);
    assert!(incremental::assert_observation(&e, &bad).is_err());
    let mut bad = a.clone();
    bad["backendCalls"][0]["event"]["missing"] = Value::Null;
    assert!(incremental::assert_observation(&e, &bad).is_err());
}

#[tokio::test]
async fn wire_adapter_uses_resolved_sdk_mapping_and_warm_call_slices() {
    // Test-only configuration variant; original fixture bytes remain untouched.
    let mut f = fixture("Q-02-repeated-correlation-multi-contract-v1");
    f["config"]["values"]["lambda-proxy.region"] = json!("eu-west-1");
    f["config"]["files"]["lambda-proxy"]["region"] = json!("${lambda-proxy.region}");
    let target = "arn:aws:lambda:eu-west-1:000000000000:function:test-only:fixture";
    f["config"]["files"]["lambda-proxy"]["functions"] = json!({"/x@get":target});
    f["backend"][0]["match"]["target"] = json!(target);
    f["expect"]["backendCalls"][0]["target"] = json!(target);
    f["config"]["values"]["light-lambda.stageVariables"] = json!({"test":"stage-value"});
    f["config"]["files"]["lambda-app"]["encodeBase64Request"] = json!(true);
    f["rawEvent"]["body"] = json!("fixture-body");
    f["rawEvent"]["stageVariables"] = json!({"untrusted":"not-copied"});
    f["expect"]["backendCalls"][0]["event"]["body"] = json!("Zml4dHVyZS1ib2R5");
    f["expect"]["backendCalls"][0]["event"]["isBase64Encoded"] = json!(true);
    let mut second = f.clone();
    second["backend"][0]["match"] =
        json!({"targets":["wrong-target",f["backend"][0]["match"]["target"]]});
    f["steps"] = json!([
        {"rawEvent":f["rawEvent"],"backend":f["backend"],"expect":f["expect"]},
        {"rawEvent":second["rawEvent"],"backend":second["backend"],"expect":second["expect"]}
    ]);
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["observations"].as_array().unwrap().len(), 2);
    for observation in r["observations"].as_array().unwrap() {
        assert_eq!(observation["backendCalls"].as_array().unwrap().len(), 1);
        let event = &observation["backendCalls"][0]["event"];
        assert_eq!(event["body"], "Zml4dHVyZS1ib2R5");
        assert_eq!(event["isBase64Encoded"], true);
        assert_eq!(event["stageVariables"], json!({"test":"stage-value"}));
    }
    assert!(!r["assertionErrors"].to_string().contains("Invoke target"));
    assert_ne!(
        r["observations"][0]["backendCalls"][0]["event"]["requestContext"]["requestId"],
        r["observations"][1]["backendCalls"][0]["event"]["requestContext"]["requestId"]
    );
}

#[tokio::test]
async fn native_ingress_error_recording_includes_single_value_headers() {
    let f = fixture("C-06-body-on-1-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["observations"][0]["bodyBase64"], false);
    assert!(r["observations"][0]["responseMaps"]["headers"].is_object());
}

#[tokio::test]
async fn supplementary_router_forwarding_uses_real_init_and_http_backend() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/incremental-router-v1");
    let files = fixture_files(&root).unwrap();
    assert_eq!(files.len(), 5);
    for file in files {
        let f = load(&file).unwrap();
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(
            r["execution"], "passed",
            "{}: {}",
            f["id"], r["assertionErrors"]
        );
        incremental::compare(&f, &r, &r).unwrap();
        for (pointer, value) in [
            ("/observations/0/httpCalls/0/uri", json!("/wrong")),
            ("/observations/0/httpCalls/0/body", json!("wrong")),
            (
                "/observations/0/httpCalls/0/target",
                json!("http://example.com:80/base/x"),
            ),
            ("/observations/0/httpCalls", json!([])),
        ] {
            let mut bad = r.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(incremental::compare(&f, &r, &bad).is_err(), "{pointer}");
        }
    }
}

#[test]
fn differential_backend_count_is_declared_and_both_expectations_are_exact() {
    let no_calls = json!({"outcome":"response","status":200,"body":"ok","bodyBase64":false,"headers":[],"headersAbsent":[],"jwksFetches":0,"backendCalls":[]});
    let mut one_call = no_calls.clone();
    one_call["backendCalls"] = json!([{"target":"Fn","event":{}}]);
    let mut f = json!({"expectJava":no_calls,"expect":one_call,"nondeterministic":[],"javaOnly":false,"rustOnly":false,"differences":["backendCalls"]});
    let j = json!({"execution":"passed","observations":[no_calls]});
    let r = json!({"execution":"passed","observations":[one_call]});
    incremental::compare(&f, &j, &r).unwrap();
    f["differences"] = json!([]);
    let err = incremental::compare(&f, &j, &r).unwrap_err();
    assert!(
        err.downcast_ref::<incremental::DifferentialMismatch>()
            .unwrap()
            .unexpected
            .contains("backendCalls")
    );
    f["differences"] = json!(["backendCalls"]);
    let mut bad = r.clone();
    bad["observations"][0]["backendCalls"] = json!([]);
    assert!(incremental::compare(&f, &j, &bad).is_err());
}

#[test]
fn selected_router_rewrite_profile_records_init_rejection_as_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("rewrite");
    assert!(
        incremental::command(&[
            "run".into(),
            "--fixture".into(),
            "H-01-no-service-loopback-v1".into(),
            "--out".into(),
            out.to_string_lossy().into()
        ])
        .is_err()
    );
    let c = load(&out.join("coverage.json")).unwrap();
    assert_eq!(c["selectedIncompleteHandlerProfiles"], 1);
    assert_eq!(c["pass"], 0);
    let r = load(&out.join("H-01-no-service-loopback-v1.json")).unwrap();
    assert_eq!(r["observations"][0]["outcome"], "initError");
}

#[test]
fn differential_jwks_counts_require_exact_expectations_and_declaration() {
    let e = json!({"outcome":"response","status":200,"body":"ok","bodyBase64":false,"headers":[],"headersAbsent":[],"jwksFetches":0,"backendCalls":[]});
    let mut fetched = e.clone();
    fetched["jwksFetches"] = json!(1);
    let mut f = json!({"expectJava":fetched,"expect":e,"nondeterministic":[],"javaOnly":false,"rustOnly":false,"differences":["jwksFetches"]});
    let j = json!({"execution":"passed","observations":[fetched]});
    let r = json!({"execution":"passed","observations":[e]});
    incremental::compare(&f, &j, &r).unwrap();
    f["differences"] = json!([]);
    assert!(incremental::compare(&f, &j, &r).is_err());
}
