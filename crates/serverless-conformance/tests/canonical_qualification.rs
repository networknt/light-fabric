use serverless_conformance::canonical_bytes;

#[test]
fn lossless_identity_matches_explicit_java_qualification_vectors() {
    for (input, expected) in [
        (
            " {\"z\":-0,\"a\":[1.00,1e+2,9007199254740993]} ",
            "{\"a\":[1.00,1e+2,9007199254740993],\"z\":-0}",
        ),
        ("{\"😀\":1,\"\u{e000}\":2}", "{\"\u{e000}\":2,\"😀\":1}"),
        (
            "{\"\\u0061\":\"\\n\",\"b\":null}",
            "{\"a\":\"\\n\",\"b\":null}",
        ),
    ] {
        assert_eq!(
            canonical_bytes(input.as_bytes()).unwrap(),
            expected.as_bytes()
        );
    }
    for invalid in [
        "{\"a\":1,\"\\u0061\":2}",
        "[1,]",
        "null true",
        "{\"x\":\"\\ud800\"}",
    ] {
        assert!(canonical_bytes(invalid.as_bytes()).is_err());
    }
    assert_ne!(
        canonical_bytes(b"1.0").unwrap(),
        canonical_bytes(b"1.00").unwrap()
    );
}
