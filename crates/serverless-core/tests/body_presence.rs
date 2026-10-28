pub mod support;
use bytes::Bytes;
use serverless_core::plan::RequestCapabilities;
use std::sync::Arc;
use support::*;
#[tokio::test]
async fn absence_empty_and_binary_bytes_survive_real_pipeline() {
    let c = Arc::new(Counts::default());
    let p = pipeline(None, None, RequestCapabilities::default(), &c);
    for body in [
        None,
        Some(Bytes::new()),
        Some(Bytes::from_static(&[0, 255, 42])),
    ] {
        let mut inv = invocation();
        inv.body = body.clone();
        let resp = p.execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(resp.body, body);
    }
    assert_eq!(n(&c.backend), 3);
    assert_eq!(n(&c.finalized), 3);
}
