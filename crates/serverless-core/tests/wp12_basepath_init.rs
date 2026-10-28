use gateway_policy::openapi_operation::{PROFILE_ERROR, SourceFormat};
use serde_json::json;
use serverless_core::{InitError, handlers::specification::SpecificationHandler};

#[test]
fn qualified_source_constructor_enforces_effective_profile_without_execution() {
    let source = br#"{"openapi":"3.0.3","paths":{},"servers":[{"url":"/api"}]}"#;
    assert!(SpecificationHandler::from_source(source, SourceFormat::Json).is_ok());
    assert!(
        SpecificationHandler::from_source_with_base_path(
            source,
            SourceFormat::Json,
            Some(&json!("(?=secret)"))
        )
        .is_ok()
    );
    assert_eq!(
        SpecificationHandler::from_source_with_base_path(
            source,
            SourceFormat::Json,
            Some(&json!(42))
        )
        .err(),
        Some(InitError::Malformed(PROFILE_ERROR))
    );
    let excluded = br#"{"openapi":"3.1.0","paths":{},"servers":[{"url":"/secret(?=/)"}]}"#;
    assert_eq!(
        SpecificationHandler::from_source(excluded, SourceFormat::Json).err(),
        Some(InitError::Malformed(PROFILE_ERROR))
    );
    let fallback =
        br#"{"openapi":"3.1.0","paths":{},"servers":[{"url":"https://never-contact.invalid"}]}"#;
    assert!(
        SpecificationHandler::from_source_with_base_path(
            fallback,
            SourceFormat::Json,
            Some(&json!("/api"))
        )
        .is_ok()
    );
    assert_eq!(
        SpecificationHandler::from_source_with_base_path(
            fallback,
            SourceFormat::Json,
            Some(&json!("/secret$"))
        )
        .err(),
        Some(InitError::Malformed(PROFILE_ERROR))
    );
    // Metadata-only compatibility deliberately does not reinterpret base paths.
    assert!(
        SpecificationHandler::from_document(
            &json!({"openapi":"3.0.3","paths":{},"servers":[{"url":"/secret$"}]})
        )
        .is_ok()
    );
}
