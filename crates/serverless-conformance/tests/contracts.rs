use serde_json::{Value, json};
use serverless_conformance::{
    canonical, canonical_bytes, cross_validate, fixture_digest, ledger_rows, load, manifest,
    schema_validate,
};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance")
}
fn fixture() -> Value {
    load(&root().join("fixtures/v1/c/C-01-isolation-a-cp1-e02-accepted.json")).unwrap()
}
fn schema() -> Value {
    load(&root().join("schema/fixture-v1.schema.json")).unwrap()
}

#[test]
fn audit_sink_is_explicit_closed_and_has_declared_differences() {
    let mut f = load(&root().join("fixtures/v1/a/A-01-selected-audit-sink-init-v1.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    assert!(valid(&f));
    assert!(cross_validate(&f, &rows).is_ok());
    f["steps"][0]
        .as_object_mut()
        .unwrap()
        .remove("observeAudit");
    assert!(cross_validate(&f, &rows).is_err());
    f["steps"][0]["observeAudit"] = json!(true);
    f["steps"][0]["expectJava"]["auditRecords"][0]["data"]["unreviewed"] = json!("hidden");
    assert!(!valid(&f));
    f["steps"][0]["expectJava"]["auditRecords"][0]["data"]
        .as_object_mut()
        .unwrap()
        .remove("unreviewed");
    f["config"]["files"]["audit"]["timestampFormat"] = json!("yyyy");
    assert!(cross_validate(&f, &rows).is_err());
    f["config"]["files"]["audit"]["timestampFormat"] = json!("'synthetic-time'");
    f["differences"]
        .as_array_mut()
        .unwrap()
        .retain(|v| v != "auditRecords");
    assert!(cross_validate(&f, &rows).is_err());
}
fn valid(v: &Value) -> bool {
    schema_validate(&schema(), v).is_ok()
}

#[test]
fn header_declarations_ignore_only_name_case_preserving_values_and_order() {
    let mut f = load(&root().join("fixtures/v1/e/E-03-response-off-0-multi-v1.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    f["expectJava"] = f["expect"].clone();
    f["expectJava"]["headers"] = json!([
        ["Content-Type", "application/json"],
        ["X-Test", "first"],
        ["X-Test", "second"]
    ]);
    f["expect"]["headers"] = json!([
        ["content-type", "application/json"],
        ["x-test", "first"],
        ["x-test", "second"]
    ]);
    f["differences"] = json!(["backendCalls"]);
    assert!(valid(&f));
    assert!(cross_validate(&f, &rows).is_ok());
    for headers in [
        json!([
            ["content-type", "text/plain"],
            ["x-test", "first"],
            ["x-test", "second"]
        ]),
        json!([
            ["content-type", "application/json"],
            ["x-test", "second"],
            ["x-test", "first"]
        ]),
        json!([["content-type", "application/json"], ["x-test", "first"]]),
    ] {
        f["expect"]["headers"] = headers;
        assert!(cross_validate(&f, &rows).is_err());
    }
    f["differences"] = json!(["backendCalls", "header:x-test"]);
    assert!(cross_validate(&f, &rows).is_ok());
}

#[test]
fn stream_component_cannot_replace_an_alb_request_or_http_response() {
    let mut f =
        load(&root().join("fixtures/v1/c/C-07-malformed-stream-fallback-component-v1.json"))
            .unwrap();
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    assert!(valid(&f));
    assert!(cross_validate(&f, &rows).is_ok());
    f["rawEvent"] = json!({"path":"/x"});
    assert!(!valid(&f));
    f.as_object_mut().unwrap().remove("rawEvent");
    f["ledger"] = json!(["C-01"]);
    assert!(!valid(&f));
    assert!(cross_validate(&f, &rows).is_err());
    f["ledger"] = json!(["C-07"]);
    f["javaOnly"] = json!(false);
    assert!(!valid(&f));
}

#[test]
fn introspection_expiry_recipe_is_relative_narrow_and_explicit() {
    let mut f =
        load(&root().join("fixtures/v1/s/S-15-introspection-0-deferred-init-v1.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    assert!(valid(&f));
    assert!(cross_validate(&f, &rows).is_ok());
    f["dependencies"]["http"]["responses"][0]["relativeIntrospectionExpSec"] = json!(0);
    assert!(!valid(&f));
    f["dependencies"]["http"]["responses"][0]["relativeIntrospectionExpSec"] = json!(600);
    f["dependencies"]["http"]["responses"][0]["body"] = json!(
        "{\"active\":true,\"client_id\":\"test-client\",\"iss\":\"test-issuer\",\"exp\":1760000000}"
    );
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn canonical_preserves_arrays_scalars_and_utf8() {
    let a = json!({"z":{"b":null,"a":"雪😀\n"},"a":[1,true,"",null]});
    let b: Value =
        serde_json::from_str(r#"{"a":[1,true,"",null],"z":{"a":"雪😀\n","b":null}}"#).unwrap();
    assert_eq!(canonical(&a).unwrap(), canonical(&b).unwrap());
    assert_eq!(
        String::from_utf8(canonical(&a).unwrap()).unwrap(),
        "{\"a\":[1,true,\"\",null],\"z\":{\"a\":\"雪😀\\n\",\"b\":null}}"
    );
    assert_ne!(
        fixture_digest(&json!([1, 2])).unwrap(),
        fixture_digest(&json!([2, 1])).unwrap()
    );
    assert_ne!(
        fixture_digest(&json!({"body":null})).unwrap(),
        fixture_digest(&json!({"body":""})).unwrap()
    );
    assert_ne!(
        fixture_digest(&json!({})).unwrap(),
        fixture_digest(&json!({"body":null})).unwrap()
    );
}

#[test]
fn lossless_canonical_vectors_match_java_and_reject_ambiguous_input() {
    use sha2::{Digest, Sha256};
    let vectors = load(&root().join("review/canonical-vectors.json")).unwrap();
    for vector in vectors.as_array().unwrap() {
        let bytes = canonical_bytes(vector["input"].as_str().unwrap().as_bytes());
        if vector["invalid"] == true {
            assert!(bytes.is_err(), "accepted {vector}");
        } else {
            let bytes = bytes.unwrap();
            assert_eq!(
                std::str::from_utf8(&bytes).unwrap(),
                vector["canonical"].as_str().unwrap()
            );
            assert_eq!(
                hex::encode(Sha256::digest(&bytes)),
                vector["sha256"].as_str().unwrap()
            );
        }
    }
    assert_ne!(
        canonical_bytes(b"1.0000000000000000001").unwrap(),
        canonical_bytes(b"1.0000000000000000002").unwrap()
    );
    assert_ne!(
        canonical_bytes(b"-0").unwrap(),
        canonical_bytes(b"0").unwrap()
    );
    assert!(canonical_bytes(b"\xff").is_err());
}

#[test]
fn request_envelopes_exclusive_and_mode_required() {
    let mut f = fixture();
    assert!(valid(&f));
    f["request"] = json!({"method":"GET","path":"/x","query":[],"headers":[],"bodyBase64":false});
    assert!(!valid(&f));
    f.as_object_mut().unwrap().remove("rawEvent");
    f.as_object_mut().unwrap().remove("headerMode");
    assert!(!valid(&f));
    f["headerMode"] = json!("multi");
    assert!(valid(&f));
    f["steps"] = json!([{"request":f["request"],"backend":[],"expect":f["expect"],"expectJava":f["expectJava"]}]);
    for key in ["request", "backend", "expect", "expectJava"] {
        f.as_object_mut().unwrap().remove(key);
    }
    assert!(!valid(&f));
    f["steps"][0]["headerMode"] = json!("multi");
    assert!(valid(&f));
}

#[test]
fn kinds_and_differences_are_enforced() {
    let mut f = fixture();
    f["javaOnly"] = json!(true);
    assert!(!valid(&f));
    f.as_object_mut().unwrap().remove("expectJava");
    f.as_object_mut().unwrap().remove("differences");
    assert!(valid(&f));
    f["rustOnly"] = json!(true);
    assert!(!valid(&f));
    f = fixture();
    f["differences"] = json!([]);
    assert!(!valid(&f));
    f = fixture();
    f["differences"] = json!(["authorization"]);
    assert!(!valid(&f));
    f = fixture();
    f.as_object_mut().unwrap().remove("expectJava");
    assert!(!valid(&f));
    f.as_object_mut().unwrap().remove("differences");
    assert!(valid(&f));
}

#[test]
fn gaps_are_explicit_and_no_nondeterministic_authorization_escape() {
    let mut f = fixture();
    f["expectJava"] = json!({"outcome":"exception","phase":"init","class":"java.lang.IllegalArgumentException","message":"test-only","backendCalls":[],"jwksFetches":0});
    f["expect"] =
        json!({"outcome":"initError","code":"UNKNOWN_ALIAS","backendCalls":[],"jwksFetches":0});
    f["differences"] = json!(["outcome"]);
    assert!(valid(&f));
    f["expect"]["status"] = json!(500);
    assert!(!valid(&f));
    f = fixture();
    f["nondeterministic"] = json!(["field:/authorization"]);
    assert!(!valid(&f));
    f = fixture();
    f["expectJava"]["status"] = Value::Null;
    f["expectJava"]["bodyBase64"] = Value::Null;
    assert!(valid(&f));
    f["expectJava"]["headers"] =
        json!([["Origin", null], ["set-cookie", "a"], ["set-cookie", "b"]]);
    assert!(valid(&f));
}

#[test]
fn cross_file_ledger_keys_and_difference_declarations() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let mut f = fixture();
    assert!(cross_validate(&f, &rows).is_ok());
    f["ledger"] = json!(["BAD-999"]);
    assert!(cross_validate(&f, &rows).is_err());
    f["ledger"] = json!(["C-01"]);
    assert!(cross_validate(&f, &rows).is_err());
    f = fixture();
    f["differences"] = json!(["status"]);
    assert!(cross_validate(&f, &rows).is_err());
    f = fixture();
    f["tokens"] = json!({"t":{"kid":"missing","claims":{},"issuedOffsetSec":-1,"expOffsetSec":10}});
    assert!(cross_validate(&f, &rows).is_err());
    f["keys"] = json!({"missing":{"alg":"RS256"}});
    assert!(cross_validate(&f, &rows).is_ok());
    f["tokens"]["t"]["claims"]["exp"] = json!(1);
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn shared_stubs_require_exact_targets_and_maximum_runner_call_count() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let mut f = fixture();
    assert!(cross_validate(&f, &rows).is_ok());
    f["backend"][0]["match"]["target"] = json!("WrongTarget");
    assert!(cross_validate(&f, &rows).is_err());
    f = fixture();
    let stub = f["backend"][0].clone();
    f["backend"].as_array_mut().unwrap().push(stub);
    assert!(cross_validate(&f, &rows).is_err());
    f = fixture();
    f["backend"] = json!([]);
    assert!(cross_validate(&f, &rows).is_err());
    f = fixture();
    f["expect"] = f["expectJava"].clone();
    // Keep the maximum-runner-count test while respecting the Rust envelope contract.
    f["expect"]["backendCalls"][0]["event"]["headers"] = json!(null);
    f["expect"]["backendCalls"][0]["event"]["queryStringParameters"] = json!(null);
    f["expectJava"]["backendCalls"] = json!([]);
    assert!(cross_validate(&f, &rows).is_ok());
    f["expect"]["backendCalls"][0]["target"] = json!("WrongTarget");
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn exact_backend_fields_and_absence_are_explicit_assertions() {
    let mut f = fixture();
    f["expectJava"]["backendCalls"][0]["eventAbsent"] = json!(["/body", "/requestContext/stage"]);
    f["expectJava"]["backendCalls"][0]["eventExact"] =
        json!({"multiValueHeaders":{"x":["a","b"]},"headers":null});
    assert!(valid(&f));
    f["expectJava"]["backendCalls"][0]["eventAbsent"] = json!(["body"]);
    assert!(!valid(&f));
    f["expectJava"]["backendCalls"][0]["eventAbsent"] = json!(["/body"]);
    f["expectJava"]["backendCalls"][0]["eventExact"] = json!([]);
    assert!(!valid(&f));
}

#[test]
fn http_dependencies_assert_call_counts_targets_and_protocol_order() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let original = load(&root().join("fixtures/v1/h/H-03-http-0-loopback-v1.json")).unwrap();
    assert!(valid(&original));
    assert!(cross_validate(&original, &rows).is_ok());
    for field in ["target", "method", "uri"] {
        let mut f = original.clone();
        f["expectJava"]["httpCalls"][0][field] = json!("wrong");
        assert!(cross_validate(&f, &rows).is_err());
    }
    let mut f = original.clone();
    f["expectJava"].as_object_mut().unwrap().remove("httpCalls");
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["dependencies"]["http"]["responses"] = json!([]);
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    let extra = f["dependencies"]["http"]["responses"][0].clone();
    f["dependencies"]["http"]["responses"]
        .as_array_mut()
        .unwrap()
        .push(extra);
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["differences"]
        .as_array_mut()
        .unwrap()
        .retain(|s| s != "httpCalls");
    assert!(cross_validate(&f, &rows).is_err());
    f = original;
    f["dependencies"]["http"]["externalUrl"] = json!("https://example.invalid");
    assert!(!valid(&f));
}

#[test]
fn warm_init_failure_is_once_and_unreached_steps_are_explicit() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root().join("fixtures/v1/s/S-18-cached-expiry-warm-jks-init-v1.json")).unwrap();
    assert!(valid(&original));
    assert!(cross_validate(&original, &rows).is_ok());
    let mut f = original.clone();
    f.as_object_mut().unwrap().remove("expectRustInit");
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["steps"][0]["expect"] = f["steps"][0]["expectJava"].clone();
    assert!(!valid(&f));
    f = original.clone();
    f["steps"][1]["waitBeforeMs"] = json!(5001);
    assert!(!valid(&f));
    f = original.clone();
    f["tokens"]["warm-expiry"]["signAtStep"] = json!(1);
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["tokens"]["warm-expiry"]["signingKey"] = json!("missing");
    assert!(cross_validate(&f, &rows).is_err());
    f = original;
    f["dependencies"]["jwks"]["publishedKeys"] = json!(["missing"]);
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn cache_mutations_are_closed_declared_and_tied_to_existing_recipes() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root().join("fixtures/v1/s/S-17-controlled-cached-nbf-mutation-v1.json")).unwrap();
    assert!(valid(&original));
    assert!(cross_validate(&original, &rows).is_ok());
    let mut f = original.clone();
    f["steps"][1]["cacheActions"][0]["kind"] = json!("arbitraryHook");
    assert!(!valid(&f));
    f = original.clone();
    f["steps"][1]["cacheActions"][0]["tokenRecipe"] = json!("missing");
    f["steps"][1]["expectJava"]["cacheActions"][0]["tokenRecipe"] = json!("missing");
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["steps"][1]["expectJava"]["cacheActions"] = json!([]);
    assert!(cross_validate(&f, &rows).is_err());
    f = original.clone();
    f["steps"][1]["cacheActions"][0]["nbfOffsetSec"] = json!(-1);
    assert!(!valid(&f));
    f = original.clone();
    f["steps"][1]["expect"]["cacheActions"] = f["steps"][1]["expectJava"]["cacheActions"].clone();
    assert!(!valid(&f));
    f = original;
    f["ledger"] = json!(["CFG-02", "CFG-04"]);
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn result_schema_distinguishes_skip_and_steps() {
    let schema = load(&root().join("schema/result-v1.schema.json")).unwrap();
    let mut r = json!({"schema":"light-lambda-conformance/result/v1","id":"example","runner":"java","runnerRevision":"0".repeat(40),"fixtureDigest":"1".repeat(64),"execution":"skipped","reason":"rustOnly","durationMs":0,"observations":[],"assertionErrors":[]});
    assert!(schema_validate(&schema, &r).is_ok());
    r["observations"] = json!([fixture()["expect"]]);
    assert!(schema_validate(&schema, &r).is_err());
    r["execution"] = json!("passed");
    assert!(schema_validate(&schema, &r).is_ok());
    r["observations"] = json!([]);
    assert!(schema_validate(&schema, &r).is_err());
    r["execution"] = json!("runnerError");
    assert!(schema_validate(&schema, &r).is_err());
    r["assertionErrors"] = json!(["invalid fixture"]);
    assert!(schema_validate(&schema, &r).is_ok());
}

#[test]
fn manifest_order_and_reproducibility() {
    let dir = std::env::temp_dir().join(format!("test-only-conformance-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::create_dir(dir.join("z")).unwrap();
    fs::create_dir(dir.join("a")).unwrap();
    fs::write(dir.join("z/Z.json"), "{\"b\":2,\"a\":1}").unwrap();
    fs::write(dir.join("a/A.json"), "[1,2]").unwrap();
    let first = manifest(&dir).unwrap();
    fs::write(dir.join("DIGEST"), &first).unwrap();
    fs::write(dir.join("z/Z.json"), "{ \"a\": 1, \"b\": 2 }").unwrap();
    assert_eq!(first, manifest(&dir).unwrap());
    assert!(first.lines().next().unwrap().ends_with("a/A.json"));
    fs::write(dir.join("z/Z.json"), "{\"a\":2,\"b\":1}").unwrap();
    assert_ne!(first, manifest(&dir).unwrap());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn server_info_structured_body_is_narrow_and_exclusive() {
    let rows = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    let mut f = fixture();
    f["ledger"] = json!(["X-02", "X-02a", "E-13"]);
    f["expectJava"].as_object_mut().unwrap().remove("body");
    f["expectJava"]["bodyBase64"] = json!(false);
    f["expectJava"]["bodyJson"] = json!({"kind":"serverInfoJavaHeap","stable":{"environment":{"runtime":{"availableProcessors":2}}}});
    assert!(valid(&f));
    assert!(cross_validate(&f, &rows).is_ok());
    f["expectJava"]["body"] = json!("historical bytes");
    assert!(!valid(&f));
    f["expectJava"].as_object_mut().unwrap().remove("body");
    f["expectJava"]["bodyBase64"] = json!(true);
    assert!(!valid(&f));
    f["expectJava"]["bodyBase64"] = json!(false);
    for name in ["freeMemory", "totalMemory", "maxMemory"] {
        f["expectJava"]["bodyJson"]["stable"]["environment"]["runtime"][name] = json!(123);
        assert!(!valid(&f));
        f["expectJava"]["bodyJson"]["stable"]["environment"]["runtime"]
            .as_object_mut()
            .unwrap()
            .remove(name);
    }
    f["expectJava"]["bodyJson"]["mask"] = json!(["/authorization"]);
    assert!(!valid(&f));
    f["expectJava"]["bodyJson"]
        .as_object_mut()
        .unwrap()
        .remove("mask");
    f["ledger"] = json!(["E-13"]);
    assert!(cross_validate(&f, &rows).is_err());
    f["ledger"] = json!(["X-02a", "E-13"]);
    f["expect"] = f["expectJava"].clone();
    assert!(cross_validate(&f, &rows).is_err());
}

#[test]
fn explicit_sdk_failures_do_not_conflate_null_payload_and_json_null() {
    let mut f = fixture();
    f["backend"][0]["respond"]["payload"] = Value::Null;
    f["backend"][0]["respond"]["nullPayload"] = json!(true);
    assert!(valid(&f));
    f["backend"][0]["respond"]["sdkFailure"] =
        json!({"kind":"SdkClientException","message":"synthetic"});
    assert!(!valid(&f));
    f["backend"][0]["respond"]
        .as_object_mut()
        .unwrap()
        .remove("nullPayload");
    assert!(valid(&f));
    f["backend"][0]["respond"]["payload"] = json!("null");
    assert!(!valid(&f));
    f["backend"][0]["respond"]["payload"] = Value::Null;
    f["backend"][0]["respond"]["sdkFailure"]["kind"] = json!("TooManyRequestsException");
    assert!(!valid(&f));
    f["backend"][0]["respond"]["sdkFailure"]["statusCode"] = json!(429);
    assert!(valid(&f));
    f["backend"][0]["respond"]["sdkFailure"]["kind"] = json!("arbitrary.ReflectionClass");
    assert!(!valid(&f));
}
