use gateway_policy::openapi_operation::{OperationCatalogue, SourceFormat};

/// Java OpenApiHelper retains SecurityScheme source order, and JwtVerifyMiddleware
/// chooses the first OAuth2 name present in the first applicable requirement.
/// Preserve this failing representation regression until that order is retained
/// by the trusted Rust metadata producer; do not activate Specification first.
#[test]
fn operation_security_scheme_order_preserves_java_scope_selection() {
    let source = r#"{
        "openapi":"3.0.3",
        "paths":{"/x":{"get":{"security":[{"zOAuth":["write"],"aOAuth":["read"]}]}}},
        "components":{"securitySchemes":{
            "zOAuth":{"type":"oauth2"},
            "aOAuth":{"type":"oauth2"}
        }}
    }"#;
    let catalogue = OperationCatalogue::from_source(source.as_bytes(), SourceFormat::Json).unwrap();
    let operation = catalogue.operation("/x", "get").unwrap();
    let schemes = operation.ordered_oauth2_names().unwrap();
    assert_eq!(
        schemes.iter().map(String::as_str).collect::<Vec<_>>(),
        ["zOAuth", "aOAuth"],
        "trusted operation metadata must preserve the order used by Java scope selection"
    );
}

#[test]
fn raw_json_yaml_orders_requirements_and_snapshot_provenance() {
    for (format, source) in [
        (
            SourceFormat::Json,
            r#"{"openapi":"3.0.3","security":[{"other":[]},{"a":["read"],"z":["write"]},{"z":["admin"]}],"paths":{"/x":{"get":{}}},"components":{"securitySchemes":{"z":{"type":"oauth2"},"other":{"type":"http","scheme":"basic"},"a":{"type":"oauth2"}}}}"#,
        ),
        (
            SourceFormat::Yaml,
            "openapi: 3.1.0\nsecurity:\n - other: []\n - a: [read]\n   z: [write]\n - z: [admin]\npaths:\n /x:\n  get: {}\ncomponents:\n securitySchemes:\n  z: {type: oauth2}\n  other: {type: http, scheme: basic}\n  a: {type: oauth2}\n",
        ),
    ] {
        for reversed in [false, true] {
            let source = if reversed {
                source
                    .replace(
                        "\"z\":{\"type\":\"oauth2\"}",
                        "\"swap\":{\"type\":\"oauth2\"}",
                    )
                    .replace("\"a\":{\"type\":\"oauth2\"}", "\"z\":{\"type\":\"oauth2\"}")
                    .replace(
                        "\"swap\":{\"type\":\"oauth2\"}",
                        "\"a\":{\"type\":\"oauth2\"}",
                    )
                    .replace("  z: {type: oauth2}", "  swap: {type: oauth2}")
                    .replace("  a: {type: oauth2}", "  z: {type: oauth2}")
                    .replace("  swap: {type: oauth2}", "  a: {type: oauth2}")
            } else {
                source.into()
            };
            let mut bytes = source.into_bytes();
            let catalogue = OperationCatalogue::from_source(&bytes, format).unwrap();
            bytes.fill(b'x');
            let op = catalogue.operation("/x", "get").unwrap();
            let expected = if reversed { ["a", "z"] } else { ["z", "a"] };
            assert_eq!(op.ordered_oauth2_names().unwrap(), expected);
            let requirements = op.security_requirements().unwrap();
            assert_eq!(requirements.len(), 3);
            assert!(requirements[0].contains_key("other"));
            assert_eq!(requirements[1]["z"], ["write"]);
            assert_eq!(requirements[2]["z"], ["admin"]);
            let metadata_only = OperationCatalogue::from_document(op.document()).unwrap();
            assert!(
                metadata_only
                    .operation("/x", "get")
                    .unwrap()
                    .ordered_oauth2_names()
                    .is_none()
            );
        }
    }
}

#[test]
fn source_parser_bounds_and_static_redacted_errors() {
    use gateway_policy::openapi_operation::MAX_SOURCE_BYTES;
    for format in [SourceFormat::Json, SourceFormat::Yaml] {
        let source = b"{\"openapi\":\"3.0.3\",\"paths\":{}}";
        let mut boundary = source.to_vec();
        boundary.resize(MAX_SOURCE_BYTES, b' ');
        assert!(OperationCatalogue::from_source(&boundary, format).is_ok());
        boundary.push(b' ');
        assert!(OperationCatalogue::from_source(&boundary, format).is_err());
        for bad in [
            "SECRET-SOURCE: [",
            "{\"openapi\":\"3.0.3\",\"openapi\":\"3.1.0\",\"paths\":{}}",
        ] {
            let error = OperationCatalogue::from_source(bad.as_bytes(), format)
                .err()
                .unwrap();
            assert!(!error.contains("SECRET-SOURCE"));
        }
        for depth in [63, 64] {
            let nested = format!(
                "{{\"openapi\":\"3.0.3\",\"paths\":{{}},\"x-bounded\":{}0{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            );
            let result = OperationCatalogue::from_source(nested.as_bytes(), format);
            assert_eq!(
                result.is_ok(),
                depth == 63,
                "scalar depth64 admitted,65 rejected"
            );
        }
        for nodes in [65_532, 65_533] {
            let expanded = format!(
                "{{\"openapi\":\"3.0.3\",\"paths\":{{}},\"x-bounded\":[{}]}}",
                vec!["0"; nodes].join(",")
            );
            assert_eq!(
                OperationCatalogue::from_source(expanded.as_bytes(), format).is_ok(),
                nodes == 65_532,
                "total expanded nodes65536 admitted,65537 rejected"
            );
        }
    }
    let refs = br##"{"openapi":"3.0.3","paths":{"/x":{"get":{}}},"components":{"securitySchemes":{"alias":{"$ref":"#/components/securitySchemes/z"},"z":{"type":"oauth2"}}}}"##;
    let op = OperationCatalogue::from_source(refs, SourceFormat::Json)
        .unwrap()
        .operation("/x", "get")
        .unwrap();
    assert_eq!(op.ordered_oauth2_names().unwrap(), ["alias", "z"]);
    let remote = String::from_utf8(refs.to_vec()).unwrap().replace(
        "#/components/securitySchemes/z",
        "https://SECRET-SOURCE.invalid/schema",
    );
    let error = OperationCatalogue::from_source(remote.as_bytes(), SourceFormat::Json)
        .err()
        .unwrap();
    assert!(!error.contains("SECRET-SOURCE"));
}

#[test]
fn yaml_alias_expansion_is_bounded_and_local_alias_order_retained() {
    let source = b"openapi: 3.1.0\npaths: { /x: { get: {} } }\ncomponents:\n securitySchemes:\n  z: &scheme {type: oauth2}\n  a: *scheme\n";
    let catalogue = OperationCatalogue::from_source(source, SourceFormat::Yaml).unwrap();
    assert_eq!(
        catalogue
            .operation("/x", "get")
            .unwrap()
            .ordered_oauth2_names()
            .unwrap(),
        ["z", "a"]
    );
    let source = format!(
        "openapi: 3.1.0\npaths: {{}}\nx-anchor: &many [{}]\nx-expanded: [{}]\n",
        vec!["0"; 1000].join(","),
        vec!["*many"; 100].join(",")
    );
    let error = OperationCatalogue::from_source(source.as_bytes(), SourceFormat::Yaml)
        .err()
        .unwrap();
    assert_eq!(error, "specification: invalid or over-bound YAML source");
    let strings = format!(
        "openapi: 3.1.0\npaths: {{}}\nx-anchor: &many {}\nx-expanded: [{}]\n",
        "a".repeat(1024),
        vec!["*many"; 1100].join(",")
    );
    assert_eq!(
        OperationCatalogue::from_source(strings.as_bytes(), SourceFormat::Yaml)
            .err()
            .unwrap(),
        "specification: invalid or over-bound YAML source"
    );
}
