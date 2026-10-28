use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serverless_core::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct NoDispatch;
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        panic!("Init must not dispatch")
    }
}
fn files(dir: &std::path::Path, token: &str, client: &str, cache: &str) {
    for (name, content) in [
        ("values.yml", "light-lambda.stage: test-only\n"),
        ("token.yml", token),
        ("client.yml", client),
        ("cache.yml", cache),
        (
            "security.yml",
            "enabled: false\nenableVerifyJwt: false\nenableVerifyScope: false\n",
        ),
        (
            "unified-security.yml",
            "enabled: false\npathPrefixAuths: []\n",
        ),
        (
            "lambda-proxy.yml",
            "enabled: true\nregion: us-east-1\nmetricsInjection: false\nlogType: None\nfunctions: {'/x@get': TestFn}\n",
        ),
        (
            "handler.yml",
            "enabled: true\nhandlers: ['com.networknt.aws.lambda.handler.middleware.token.TokenMiddleware@token','com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy']\ndefaultHandlers: [token,proxy]\n",
        ),
    ] {
        std::fs::write(dir.join(name), content).unwrap();
    }
}
fn client(url: &str) -> String {
    format!(
        "oauth:\n  token:\n    server_url: '{url}'\n    enableHttp2: false\n    client_credentials:\n      client_id: test-only-client\n      client_secret: secret-must-not-leak\n      uri: /token\nrequest:\n  timeout: 1000\n"
    )
}
#[tokio::test]
async fn token_offline_startup_per_key_validation_and_redaction_match() {
    for (token,client,cache,valid) in [
        ("enabled: true\nappliedPathPrefixes: ['/x']",client("https://provider.example"),"enabled: false\ncaches: [{cacheName: token,maxSize: 100,expiryInMinutes: 15}]",true),
        ("enabled: false",String::new(),"enabled: false",true),
        ("enabled: true\nappliedPathPrefixes: 4",client("https://provider.example"),"enabled: false",false),
        ("enabled: false\nunknown: true",String::new(),"enabled: false",false),
        ("enabled: false","request: {errorThreshold: 1}".into(),"enabled: false",false),
        ("enabled: false","oauth: {token: {token_exchange: {client_secret: secret-must-not-leak}}}".into(),"enabled: false",false),
        ("enabled: true",client("https://user:secret-must-not-leak@provider.example"),"enabled: false",false),
        ("enabled: true",client("http://provider.example"),"enabled: false",false),
        ("enabled: true",client("https://provider.example"),"enabled: false\ncaches: [{cacheName: token,maxSize: -1,expiryInMinutes: 15}]",false),
        ("enabled: true",client("https://provider.example"),"enabled: false\ncaches: [{cacheName: token,maxSize: 1,expiryInMinutes: 1},{cacheName: token,maxSize: 1,expiryInMinutes: 1}]",false),
        ("enabled: true",client("https://provider.example"),"enabled: true",false),
        ("enabled: false","oauth: {token: {client_credentials: {serviceIdAuthServers: {fixed: {unknown: true}}}}}".into(),"enabled: false",false),
    ]{
        let dir=tempfile::tempdir().unwrap();files(dir.path(),token,&client,cache);
        let report=validate_configuration(&light_lambda::input(Some(dir.path().into()),false).unwrap()).await;
        let mut input=light_lambda::input(Some(dir.path().into()),false).unwrap();input.backend=BackendConfiguration::Injected(Arc::new(NoDispatch));
        let result=initialize(input).await;
        assert_eq!(report.valid,valid,"{token}: {:?}",report.errors);assert_eq!(result.is_ok(),valid);
        if valid{assert_eq!(report.config_digest.as_deref(),Some(result.unwrap().config_digest()));assert!(report.warnings.iter().any(|v|v.starts_with("client.request.errorThreshold:")));}
        else{assert_eq!(report.errors,vec![result.err().unwrap().message().to_owned()]);}
        assert!(!serde_json::to_string(&report).unwrap().contains("secret-must-not-leak"));
    }
}
#[tokio::test]
async fn offline_and_startup_never_contact_the_configured_token_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0; 100];
            let _ = stream.read(&mut buffer).await;
            let _ = stream.write_all(b"invalid").await;
        }
    });
    let dir = tempfile::tempdir().unwrap();
    files(
        dir.path(),
        "enabled: true\nappliedPathPrefixes: ['/x']",
        &client(&url),
        "enabled: false",
    );
    let report =
        validate_configuration(&light_lambda::input(Some(dir.path().into()), false).unwrap()).await;
    assert!(report.valid, "{:?}", report.errors);
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
    assert!(initialize(input).await.is_ok());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}
