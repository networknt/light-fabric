use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn default_follows_cross_origin_but_lambda_token_client_never_follows() {
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination_url = format!("http://{}/token", destination.local_addr().unwrap());
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_url = format!("http://{}/token", origin.local_addr().unwrap());
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: {destination_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(redirect.as_bytes()).await.unwrap();
        }
    });
    let factory =
        ClientFactory::from_parts(ClientRequestConfig::default(), ClientTlsConfig::default());
    let lambda = factory
        .reqwest_client_lambda_token_no_redirect(EndpointOptions::default())
        .unwrap();
    let response = lambda
        .post(&origin_url)
        .basic_auth("fixture", Some("fixture-secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
    let ordinary = factory.reqwest_client(EndpointOptions::default()).unwrap();
    let request = tokio::spawn(async move { ordinary.get(origin_url).send().await.unwrap() });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), destination.accept())
        .await
        .unwrap()
        .unwrap();
    let mut buffer = [0; 4096];
    assert!(stream.read(&mut buffer).await.unwrap() > 0);
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await
        .unwrap();
    assert_eq!(request.await.unwrap().status(), reqwest::StatusCode::OK);
    server.await.unwrap();
}
