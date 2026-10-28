use gateway_policy::openapi_operation::{OperationCatalogue, SourceFormat};
use std::sync::Arc;

#[test]
fn raw_source_resource_retains_exact_lexemes_and_is_shared_across_readers() {
    let source = br#"{"openapi":"3.1.0","paths":{"/x":{"post":{"requestBody":{"content":{"application/json":{"schema":{"maximum":1.00000000000000000001,"minimum":1e-30}}}}}}}}"#;
    let catalogue = OperationCatalogue::from_source(source, SourceFormat::Json).unwrap();
    let (format, first) = catalogue.source_resource().unwrap();
    let (_, second) = catalogue.source_resource().unwrap();
    assert!(matches!(format, SourceFormat::Json));
    assert_eq!(&*first, source);
    assert!(Arc::ptr_eq(&first, &second));
    let document = serde_json::from_slice(source).unwrap();
    let metadata = OperationCatalogue::from_document(&document).unwrap();
    assert!(metadata.source_resource().is_none());
    assert_eq!(
        catalogue.templates().collect::<Vec<_>>(),
        metadata.templates().collect::<Vec<_>>()
    );
}
