use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serverless_core::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct NoDispatch(Arc<AtomicUsize>);
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("metadata plumbing does not qualify Specification activation")
    }
}
#[tokio::test]
async fn metadata_stage_does_not_remove_unqualified_specification_init_restriction() {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("values.yml", "light-lambda.stage: test-only\n"),
        (
            "security.yml",
            "enabled: false\nenableVerifyJwt: false\nenableVerifyScope: false\n",
        ),
        (
            "unified-security.yml",
            "enabled: false\npathPrefixAuths: []\n",
        ),
        ("openapi-handler.yml", "enabled: true\n"),
        (
            "lambda-proxy.yml",
            "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {}\n",
        ),
        (
            "openapi.yaml",
            "openapi: 3.0.3\ninfo: {title: local-only, version: '1'}\npaths: {}\n",
        ),
    ] {
        std::fs::write(dir.path().join(file), text).unwrap();
    }
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Specification)
        .unwrap()
        .factory = None;
    let report = validate_configuration(&input).await;
    assert!(!report.valid);
    let count = Arc::new(AtomicUsize::new(0));
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Specification)
        .unwrap()
        .factory = None;
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch(count.clone())));
    let error = initialize(input)
        .await
        .err()
        .expect("Specification remains unqualified");
    assert_eq!(report.errors, vec![error.message()]);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
