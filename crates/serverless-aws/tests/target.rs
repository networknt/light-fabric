mod support;
use serverless_aws::target::{validate, validate_endpoint};
#[test]
fn configured_forms_qualifiers_accounts_regions_and_self_guard() {
    let id = support::identity();
    for t in [
        "business",
        "business:live",
        "business:42",
        "business:$LATEST",
        "000000000000:function:business:live",
        "arn:aws:lambda:us-east-1:000000000000:function:business",
        "arn:aws:lambda:us-east-1:000000000000:function:business:42",
    ] {
        assert!(validate(t, &id).is_ok(), "{t}");
    }
    for t in [
        "",
        "a/b",
        "a:",
        "a:0",
        "a:01",
        "a:x:y",
        "000000000001:function:business",
        "arn:aws:lambda:us-west-2:000000000000:function:business",
        "arn:aws-cn:lambda:us-east-1:000000000000:function:business",
        "arn:aws:lambda:us-east-1:000000000001:function:business",
        "test-only-gateway",
        "test-only-gateway:live",
        "test-only-gateway:42",
        "000000000000:function:test-only-gateway:live",
        "arn:aws:lambda:us-east-1:000000000000:function:test-only-gateway:$LATEST",
    ] {
        assert!(validate(t, &id).is_err(), "{t}");
    }
}
#[test]
fn insecure_endpoint_requires_explicit_numeric_loopback_test_switch() {
    assert!(validate_endpoint("https://lambda.example.test", false).is_ok());
    assert!(validate_endpoint("http://127.0.0.1:1234", false).is_err());
    assert!(validate_endpoint("http://127.0.0.1:1234", true).is_ok());
    assert!(validate_endpoint("http://[::1]:1234", true).is_ok());
    for e in [
        "http://lambda.amazonaws.com",
        "http://localhost:1234",
        "https://user:secret@example.test",
        "https://example.test/?token=secret",
        "file:///tmp/test",
    ] {
        assert!(validate_endpoint(e, true).is_err(), "{e}");
    }
}
