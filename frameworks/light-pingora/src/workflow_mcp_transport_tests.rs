use super::*;
use std::io::{Read, Write};

fn fixture() -> (tempfile::TempDir, light_client::workflow_actions::Config) {
    let directory = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(directory.path().join("ca.pem"), certificate.cert.pem()).unwrap();
    std::fs::write(
        directory.path().join("identity.pem"),
        format!(
            "{}{}",
            certificate.cert.pem(),
            certificate.signing_key.serialize_pem()
        ),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("scope"),
        format!("Bearer {}", "a".repeat(64)),
    )
    .unwrap();
    let config = light_client::workflow_actions::Config {
        base_url: "https://localhost:8449/".into(),
        client_identity_file: "identity.pem".into(),
        ca_file: "ca.pem".into(),
        scope_token_file: "scope".into(),
        owner: workflow_action::GatewayRegistration {
            gateway_service: "gateway".into(),
            replica: Uuid::new_v4(),
        },
    };
    (directory, config)
}

#[test]
fn workflow_mtls_rejects_unsafe_targets_and_missing_credentials() {
    let (directory, config) = fixture();
    for base in [
        "http://localhost:8449",
        "https://u:p@localhost",
        "https://localhost/path",
        "https://localhost?q=1",
        "https://localhost/#x",
    ] {
        let mut changed = config.clone();
        changed.base_url = base.into();
        assert!(workflow_action_dispatch(&changed, directory.path(), &[1]).is_err());
    }
    let mut changed = config.clone();
    changed.client_identity_file = "missing".into();
    assert!(workflow_action_dispatch(&changed, directory.path(), &[1]).is_err());
    for scope in ["", "Bearer short", "Bearer valid\r\nInjected: value"] {
        std::fs::write(directory.path().join("scope"), scope).unwrap();
        assert!(workflow_action_dispatch(&config, directory.path(), &[1]).is_err());
    }
}

#[tokio::test]
async fn workflow_mtls_uses_fixed_identity_and_does_not_follow_redirects() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (directory, mut config) = fixture();
    let pem = std::fs::read(directory.path().join("identity.pem")).unwrap();
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut pem.as_slice())
        .unwrap()
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certs[0].clone()).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let server = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.base_url = format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let receiver = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(server)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert_eq!(stream.conn.peer_certificates().unwrap().len(), 1);
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /v1/workflow-invocations/"));
        assert!(request.contains("authorization: Bearer owner"));
        assert!(request.contains(&format!("x-scope-token: Bearer {}", "a".repeat(64))));
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/credential-sink\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        stream.flush().unwrap();
    });
    let transport = workflow_action_dispatch(&config, directory.path(), &[2]).unwrap();
    assert_eq!(transport.permit_pools[0].available_permits(), 2);
    let response = transport
        .client
        .get(format!(
            "{}/v1/workflow-invocations/{}",
            transport.invocation_url,
            Uuid::new_v4()
        ))
        .header("authorization", "Bearer owner")
        .header(
            "x-scope-token",
            transport.scope_authorization.as_ref().unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    receiver.join().unwrap();
    assert!(
        transport
            .client
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .is_err()
    );
}
