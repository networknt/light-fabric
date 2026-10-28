//! Non-child metadata/admission checks. These do not qualify schema semantics.
use gateway_policy::openapi_operation::{BasePathProfile, OperationCatalogue, SourceFormat};
use light_lambda_runtime::init;

const SOURCE: &[u8] = include_bytes!("../migrations/native-petstore/openapi.yaml");

#[test]
fn native_server_prefix_normalizes_without_rewriting_forwarded_paths() {
    let profile = BasePathProfile::from_source(SOURCE, SourceFormat::Yaml, None).unwrap();
    assert_eq!(profile.normalize_bounded_path("/v1/pets").unwrap(), "/pets");
    assert_eq!(profile.normalize_bounded_path("/pets").unwrap(), "/pets");
    assert_eq!(
        profile.normalize_bounded_path("/v1/pets/17").unwrap(),
        "/pets/17"
    );
    let catalogue = OperationCatalogue::from_source(SOURCE, SourceFormat::Yaml).unwrap();
    assert!(catalogue.operation("/pets", "POST").is_some());
    assert!(catalogue.operation("/pets/{petId}", "PUT").is_some());
    assert!(catalogue.operation("/v1/pets", "POST").is_none());
    assert_eq!(catalogue.source_resource().unwrap().1.as_ref(), SOURCE);
}

#[test]
fn native_required_name_is_preserved_in_captured_schema() {
    let catalogue = OperationCatalogue::from_source(SOURCE, SourceFormat::Yaml).unwrap();
    let operation = catalogue.operation("/pets", "POST").unwrap();
    let schema = &operation.document()["components"]["schemas"]["CreatePetRequest"];
    assert_eq!(schema["required"], serde_json::json!(["name"]));
    assert_eq!(schema["properties"]["name"]["minLength"], 3);
    assert_eq!(schema["additionalProperties"], false);
    // No in-parent validation of requests is introduced by this test.
}

#[tokio::test]
async fn validator_enabled_qualification_stays_closed_in_default_application() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/native-petstore");
    let input = light_lambda::input(Some(dir), false).unwrap();
    let report = init::validate_configuration(&input).await;
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("enabled handler not supported")),
        "{:?}",
        report.errors
    );
    assert!(init::initialize(input).await.is_err());
}
