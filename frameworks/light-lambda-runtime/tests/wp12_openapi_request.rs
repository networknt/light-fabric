//! Validator component qualification, not the WP13 differential runner.
#[path = "profile_support/openapi_request.rs"]
mod adapter;
use serde_json::Value;

#[test]
fn current_header_lexemes_match_native_and_normalization_alone_is_insufficient() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-header-validator-java.json")).unwrap();
    let mut normalized_differences = 0;
    let mut ingress_hint_differences = 0;
    for row in &rows {
        let input = &row["input"];
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let mut event = input["event"].clone();
        event["headers"] = row["java"]["headersAtValidator"].clone();
        let error = prepared.validate(&event).unwrap();
        if let Some(error) = error {
            assert_eq!(row["java"]["status"], 400);
            assert_eq!(row["java"]["lambdaCaptures"], 0);
            assert_eq!(
                error.to_bytes(),
                row["java"]["body"].as_str().unwrap().as_bytes()
            );
        } else {
            assert_eq!(row["java"]["status"], 200);
            assert_eq!(row["java"]["lambdaCaptures"], 1);
        }
        let normalized: serde_json::Map<_, _> = row["java"]["headersAtValidator"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect();
        event["headers"] = Value::Object(normalized.clone());
        let normalized_status = if prepared.validate(&event).unwrap().is_some() {
            400
        } else {
            200
        };
        if row["java"]["status"] != normalized_status {
            normalized_differences += 1;
        }
        let ingress_only: serde_json::Map<_, _> = input["event"]["headers"]
            .as_object()
            .unwrap()
            .keys()
            .filter_map(|key| {
                normalized
                    .get(&key.to_ascii_lowercase())
                    .map(|value| (key.clone(), value.clone()))
            })
            .collect();
        event["headers"] = Value::Object(ingress_only);
        let ingress_status = if prepared.validate(&event).unwrap().is_some() {
            400
        } else {
            200
        };
        if row["java"]["status"] != ingress_status {
            ingress_hint_differences += 1;
        }
    }
    assert_eq!(rows.len(), 16);
    assert_eq!(normalized_differences, 6);
    assert_eq!(ingress_hint_differences, 6);
    println!(
        "CURRENT_HEADER_PROOF cases=16 normalized_changes=6 ingress_only_changes=6 production_factory=false"
    );
}

#[test]
fn approved_percent_fragment_amendment_preserves_original_observations() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-reference-uri-java.json")).unwrap();
    let mut amended = 0;
    for row in &rows {
        let input = &row["input"];
        let id = input["id"].as_str().unwrap();
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let error = prepared.validate(&input["event"]).unwrap().unwrap();
        let baseline = if id.contains("percent") {
            amended += 1;
            assert_eq!(row["java"]["status"], 200);
            assert_eq!(row["java"]["lambdaCaptures"], 1);
            let prefix = id.split_once("-percent").unwrap().0;
            rows.iter()
                .find(|r| r["input"]["id"].as_str() == Some(&format!("{prefix}-slash")))
                .unwrap()
        } else {
            row
        };
        assert_eq!(baseline["java"]["status"], 400);
        assert_eq!(
            error.to_bytes(),
            baseline["java"]["body"].as_str().unwrap().as_bytes(),
            "{id}"
        );
    }
    assert_eq!(amended, 8);
}

#[test]
fn fragment_decode_order_unicode_and_invalid_encodings_are_qualified() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-reference-uri-java.json")).unwrap();
    for dialect in ["3.0.3", "3.1.0"] {
        let row = rows
            .iter()
            .find(|r| r["input"]["id"] == format!("reference-uri-{dialect}-query-slash"))
            .unwrap();
        let input = &row["input"];
        for (key, fragment) in [
            ("a/b", "a%7E1b"),
            ("a~b", "a%7E0b"),
            ("é", "%C3%A9"),
            ("a~1b", "a%7E01b"),
        ] {
            let mut document = input["files"]["openapi"].clone();
            let parameter = document["components"]["parameters"]["a/b"].clone();
            document["components"]["parameters"] = serde_json::json!({key: parameter});
            document["paths"]["/x"]["post"]["parameters"][0]["$ref"] =
                format!("#/components/parameters/{fragment}").into();
            let prepared =
                adapter::Prepared::compile(&document, &input["files"]["openapi-validator"])
                    .unwrap();
            assert_eq!(
                prepared
                    .validate(&input["event"])
                    .unwrap()
                    .unwrap()
                    .to_bytes(),
                row["java"]["body"].as_str().unwrap().as_bytes()
            );
        }
        for fragment in [
            "%",
            "%0",
            "%GG",
            "%FF",
            "%C3%28",
            "%ED%A0%80",
            "%F4%90%80%80",
        ] {
            let mut document = input["files"]["openapi"].clone();
            document["paths"]["/x"]["post"]["parameters"][0]["$ref"] =
                format!("#/components/parameters/{fragment}").into();
            assert!(
                adapter::Prepared::compile(&document, &Value::Null).is_err(),
                "{dialect}: {fragment}"
            );
        }
    }
}

#[test]
fn characterized_keyword_violations_preserve_native_first_error_bytes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-keyword-java.json")).unwrap();
    let amendments: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-keyword-approved-amendments.json")).unwrap();
    let mut native = 0;
    let mut amended = 0;
    for row in rows {
        let input = &row["input"];
        let id = input["id"].as_str().unwrap();
        let intentional = id.contains("3.1.0")
            && [
                "contains",
                "propertyNames",
                "dependentRequired",
                "prefixItems",
            ]
            .iter()
            .any(|key| id.ends_with(key));
        let body = if intentional {
            amended += 1;
            let expected_id = format!("synthetic-typed-{id}");
            amendments.iter().find(|r| r["id"] == expected_id).unwrap()["rawSchemaEngineBody"]
                .as_str()
                .unwrap()
        } else {
            native += 1;
            row["java"]["body"].as_str().unwrap()
        };
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let error = prepared
            .validate(&input["event"])
            .unwrap_or_else(|e| panic!("{id}: {e}"))
            .unwrap();
        assert_eq!(error.to_bytes(), body.as_bytes(), "{id}");
        assert_eq!(row["java"]["status"], 400);
        assert_eq!(row["java"]["lambdaCaptures"], 0);
    }
    assert_eq!((native, amended), (36, 4));
}

#[test]
fn typed_keyword_dialects_preserve_java30_and_apply_approved31_amendments() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-keyword-typed-java.json")).unwrap();
    let amendments: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-keyword-approved-amendments.json")).unwrap();
    for row in rows {
        let input = &row["input"];
        let id = input["id"].as_str().unwrap();
        assert_eq!(row["java"]["status"], 200);
        assert_eq!(row["java"]["lambdaCaptures"], 1);
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let result = prepared
            .validate(&input["event"])
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        if id.contains("3.0.3") {
            assert!(result.is_none(), "{id}");
        } else {
            let expected = amendments.iter().find(|r| r["id"] == id).unwrap();
            assert_eq!(
                result.unwrap().to_bytes(),
                expected["rawSchemaEngineBody"].as_str().unwrap().as_bytes(),
                "{id}"
            );
        }
    }
}

#[test]
fn captured_parameter_and_request_body_references_match_native_bytes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-reference-java.json")).unwrap();
    assert_eq!(rows.len(), 4);
    for row in rows {
        let input = &row["input"];
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let error = prepared.validate(&input["event"]).unwrap().unwrap();
        assert_eq!(row["java"]["status"], 400);
        assert_eq!(row["java"]["lambdaCaptures"], 0);
        assert_eq!(
            error.to_bytes(),
            row["java"]["body"].as_str().unwrap().as_bytes(),
            "{}",
            input["id"]
        );
        assert_eq!(prepared.schema_count(), 1);
    }
}

#[test]
fn reference_object_cycles_and_uncatalogued_retrieval_fail_before_readiness() {
    let mut document = serde_json::json!({
        "openapi": "3.1.0",
        "paths": {"/x": {"post": {
            "parameters": [{"$ref": "#/components/parameters/v"}]
        }}},
        "components": {"parameters": {"v": {"$ref": "#/components/parameters/v"}}}
    });
    assert!(adapter::Prepared::compile(&document, &Value::Null).is_err());
    document["components"]["parameters"]["v"]["$ref"] =
        Value::String("https://127.0.0.1:1/not-retrieved".into());
    assert!(adapter::Prepared::compile(&document, &Value::Null).is_err());
    document["components"]["parameters"]["v"]["$ref"] =
        Value::String("file:///not-retrieved".into());
    assert!(adapter::Prepared::compile(&document, &Value::Null).is_err());
}

#[test]
fn native_missing_parameter_and_unexpected_body_bytes_are_preserved() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-missing-java.json")).unwrap();
    assert_eq!(rows.len(), 4);
    for row in rows {
        let input = &row["input"];
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        let result = prepared.validate(&input["event"]).unwrap();
        match result {
            Some(error) => {
                assert_eq!(row["java"]["status"], 400, "{}", input["id"]);
                assert_eq!(
                    error.to_bytes(),
                    row["java"]["body"].as_str().unwrap().as_bytes(),
                    "{}",
                    input["id"]
                );
            }
            None => assert_eq!(row["java"]["status"], 200, "{}", input["id"]),
        }
        assert_eq!(
            row["java"]["lambdaCaptures"],
            if row["java"]["status"] == 200 { 1 } else { 0 }
        );
    }
}
#[test]
fn actual_wp2_and_fresh_java_requests_use_real_precompiled_schema_validation() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-java-reproduction.json")).unwrap();
    let amendments: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/amendments/wp12-validator-dialects-v1.json"
    ))
    .unwrap();
    let bytes: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/amendments/wp12-validator-violation-bytes-v1.json"
    ))
    .unwrap();
    let mut qualified = 0;
    let mut differences = 0;
    for row in &rows {
        let input = &row["input"];
        let id = input["id"].as_str().unwrap();
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap_or_else(|error| panic!("{id}: {error}"));
        assert!(prepared.schema_count() > 0);
        let response = prepared
            .validate(&input["event"])
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        if let Some(amendment) = amendments["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["observationId"] == id)
        {
            assert_eq!(
                response.is_none(),
                amendment["approvedRustSchemaValid"].as_bool().unwrap(),
                "{id}"
            );
            if amendment["intentionalChange"].is_string() {
                differences += 1;
            }
            let expected = bytes["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == id)
                .unwrap();
            if let Some(response) = &response {
                assert_eq!(
                    String::from_utf8(response.to_bytes().to_vec()).unwrap(),
                    expected["body"].as_str().unwrap(),
                    "{id}"
                );
            }
        } else {
            assert_eq!(
                response.as_ref().map_or(200, |e| e.status_code),
                row["java"]["status"].as_u64().unwrap() as u16,
                "{id}"
            );
            if let Some(error) = response {
                assert_eq!(
                    String::from_utf8(error.to_bytes().to_vec()).unwrap(),
                    row["java"]["body"].as_str().unwrap(),
                    "{id}"
                );
            }
            qualified += 1;
        }
    }
    println!(
        "OPENAPI_REQUEST_QUALIFICATION {}",
        serde_json::json!({"requests":rows.len(),"unchangedBaselineCases":qualified,"approvedAmendmentCases":rows.len()-qualified,"intentionalChanges":differences,"productionAdmission":false})
    );
}
#[test]
fn exact_violation_bytes_follow_java_format_after_approved_semantic_changes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-schema-violation-bytes.json")).unwrap();
    let mut exact = 0;
    for row in rows {
        let schema = &row["input"]["schema"];
        let document = serde_json::json!({"openapi":"3.1.0","paths":{"/x":{"post":{"requestBody":{"content":{"application/json":{"schema":schema}}}}}}});
        let prepared =
            adapter::Prepared::compile(&document, &serde_json::json!({"validateRequest":true}))
                .unwrap();
        let event = serde_json::json!({"path":"/x","httpMethod":"POST","headers":{"Content-Type":"application/json"},"body":row["input"]["value"].to_string()});
        if row["input"]["id"] == "enum" {
            assert!(
                serde_json::from_str::<Value>(row["result"]["body"].as_str().unwrap()).is_err()
            );
        }
        let result = prepared.validate(&event).unwrap();
        assert_eq!(result.is_none(), row["result"]["valid"].as_bool().unwrap());
        if let Some(result) = result {
            assert_eq!(
                String::from_utf8(result.to_bytes().to_vec()).unwrap(),
                row["result"]["body"].as_str().unwrap()
            );
        }
        exact += 1;
    }
    println!(
        "OPENAPI_VIOLATION_BYTES {}",
        serde_json::json!({"qualifiedCases":exact,"unescapedEnumBytesPreserved":true})
    );
}
