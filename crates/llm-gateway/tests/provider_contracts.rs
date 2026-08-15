use base64::{Engine as _, engine::general_purpose::STANDARD};
use model_provider::conformance::{
    Ed25519EvidenceSigner, TrustedEvidenceKeySet, canonical_json_bytes, sha256_hex,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldenVector {
    schema_version: String,
    algorithm: String,
    seed_base64: String,
    public_key_base64: String,
    payload_sha256: String,
    signature: String,
    payload: Value,
}

fn vector() -> GoldenVector {
    serde_json::from_str(include_str!(
        "fixtures/provider/v1/signing-golden-vector.json"
    ))
    .expect("checked-in Ed25519 vector")
}

#[test]
fn rust_verifies_the_cross_language_ed25519_golden_vector() {
    let vector = vector();
    assert_eq!(vector.schema_version, "1");
    assert_eq!(vector.algorithm, "Ed25519");
    let seed = STANDARD.decode(vector.seed_base64).unwrap();
    let signer = Ed25519EvidenceSigner::from_seed("provider-qualification-test", &seed).unwrap();
    assert_eq!(
        STANDARD.encode(signer.public_key()),
        vector.public_key_base64
    );
    let payload = canonical_json_bytes(&vector.payload).unwrap();
    assert_eq!(sha256_hex(&payload), vector.payload_sha256);
    assert_eq!(signer.sign_bytes(&payload), vector.signature);
    let trust = TrustedEvidenceKeySet::new(
        "test-v1",
        BTreeMap::from([(
            "provider-qualification-test".to_string(),
            signer.public_key(),
        )]),
    )
    .unwrap();
    trust
        .verify("provider-qualification-test", &payload, &vector.signature)
        .unwrap();
}

#[test]
fn every_live_binding_mutation_invalidates_the_golden_signature() {
    let vector = vector();
    let public_key = STANDARD.decode(vector.public_key_base64).unwrap();
    let trust = TrustedEvidenceKeySet::new(
        "test-v1",
        BTreeMap::from([("provider-qualification-test".to_string(), public_key)]),
    )
    .unwrap();
    for pointer in [
        "/providerEndpointSha256",
        "/networkProfileSha256",
        "/deploymentRevisionId",
        "/physicalRuntimeId",
        "/capacityDeclarationSha256",
        "/sidecar/configSha256",
        "/testedOperations/0",
        "/validUntil",
        "/runnerVantage/sourceNetworkNamespaceId",
    ] {
        let mut tampered = vector.payload.clone();
        *tampered.pointer_mut(pointer).expect("golden pointer") = Value::String("tampered".into());
        assert!(
            trust
                .verify(
                    "provider-qualification-test",
                    &canonical_json_bytes(&tampered).unwrap(),
                    &vector.signature,
                )
                .is_err(),
            "mutation at {pointer} retained a valid signature"
        );
    }
}
