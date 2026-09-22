//! The issuer over a real TLS socket, with a throwaway CA and server certificate.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use light_identity_issuer::{
    CaMaterial, CombinedAuthorizer, EnvPolicy, InMemoryPairingCodes, InMemoryRevocationList,
    IssuerPolicy, OnDiskCaSigner, PairingGrantAuthorizer, PortalTokenAuthorizer, WorkloadIssuer,
};
use light_identity_issuer_service::{http, jwks::JwksCache, tls};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tempfile::TempDir;

struct Pki {
    dir: TempDir,
    ca_pem: String,
    certificate: PathBuf,
    key: PathBuf,
}

/// A CA, and a server certificate for `localhost` / `127.0.0.1` signed by it.
fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let mut server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    server_params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let dir = TempDir::new().unwrap();
    let certificate = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&certificate, server_cert.pem()).unwrap();
    std::fs::write(&key, server_key.serialize_pem()).unwrap();
    Pki {
        dir,
        ca_pem: ca_cert.pem(),
        certificate,
        key,
    }
}

fn app() -> axum::Router {
    let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
    let pairing_codes = Arc::new(InMemoryPairingCodes::new());
    let authorizer = CombinedAuthorizer::new(
        PortalTokenAuthorizer::new("i", "a", JwksCache::empty_for_tests()),
        PairingGrantAuthorizer::new(Arc::clone(&pairing_codes)),
    );
    let issuer = WorkloadIssuer::new(
        OnDiskCaSigner::new(material),
        IssuerPolicy::new().with_env(
            "dev",
            EnvPolicy::new(Duration::from_secs(3_600), Duration::from_secs(600)),
        ),
        InMemoryRevocationList::new(),
        authorizer,
    );
    http::router(Arc::new(http::AppState {
        issuer,
        pairing_codes,
        pairing_stub_enabled: false,
    }))
}

async fn serve(pki: &Pki) -> u16 {
    let config = tls::server_config(&pki.certificate, &pki.key).expect("valid pair");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = tls::serve_tls(listener, app(), config).await;
    });
    port
}

fn client_trusting(pki: &Pki) -> reqwest::Client {
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(pki.ca_pem.as_bytes()).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn a_client_that_trusts_the_ca_is_served_over_https() {
    let pki = pki();
    let port = serve(&pki).await;
    let client = client_trusting(&pki);

    let health = client
        .get(format!("https://localhost:{port}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.unwrap(), "ok");

    // The real router is behind it: an empty renewal is a 422, not a 404.
    let renew = client
        .post(format!("https://127.0.0.1:{port}/v1/renew"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(renew.status(), 422);
}

#[tokio::test]
async fn a_client_that_does_not_trust_the_ca_is_refused() {
    let pki = pki();
    let port = serve(&pki).await;
    let untrusting = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let result = untrusting
        .get(format!("https://localhost:{port}/health"))
        .send()
        .await;
    assert!(result.is_err(), "an unknown CA must fail the handshake");
}

#[tokio::test]
async fn plain_http_to_the_tls_port_gets_no_answer() {
    let pki = pki();
    let port = serve(&pki).await;
    let plain = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let result = plain
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await;
    match result {
        Err(_) => {}
        Ok(response) => assert!(
            !response.status().is_success(),
            "a cleartext request must not be served by the TLS port"
        ),
    }
}

#[test]
fn a_missing_certificate_file_is_an_error() {
    let pki = pki();
    let error = tls::server_config(&pki.dir.path().join("absent.pem"), &pki.key).unwrap_err();
    assert!(error.contains("could not read"), "{error}");
}

#[test]
fn a_file_with_no_certificate_or_no_key_is_an_error() {
    let pki = pki();
    let empty = pki.dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    assert!(
        tls::server_config(&empty, &pki.key)
            .unwrap_err()
            .contains("no certificate")
    );
    assert!(
        tls::server_config(&pki.certificate, &empty)
            .unwrap_err()
            .contains("no private key")
    );
}

#[test]
fn a_key_that_does_not_match_the_certificate_fails_at_startup() {
    let pki = pki();
    let other = pki.dir.path().join("other-key.pem");
    std::fs::write(&other, KeyPair::generate().unwrap().serialize_pem()).unwrap();
    let error = tls::server_config(&pki.certificate, &other).unwrap_err();
    assert!(error.contains("do not form a usable pair"), "{error}");
}

/// The service is PID 1 in its container: it must stop on SIGTERM (the shutdown future), not
/// wait for `docker stop` to kill it, and an idle keep-alive connection must not hold it up.
#[tokio::test]
async fn shutdown_stops_the_server_promptly_even_with_an_idle_connection_open() {
    let pki = pki();
    let config = tls::server_config(&pki.certificate, &pki.key).expect("valid pair");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        tls::serve_tls_until(
            listener,
            app(),
            config,
            async {
                let _ = stopped.await;
            },
            Duration::from_secs(1),
        )
        .await
    });

    // One request, on a client that keeps its connection open afterwards.
    let client = client_trusting(&pki);
    assert_eq!(
        client
            .get(format!("https://localhost:{port}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(!server.is_finished(), "it keeps serving until told to stop");

    let started = std::time::Instant::now();
    stop.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .expect("the server must stop soon after the signal, not hang on the idle connection");
    assert!(result.unwrap().is_ok());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    // And it no longer accepts connections.
    let fresh = client_trusting(&pki);
    assert!(
        fresh
            .get(format!("https://localhost:{port}/health"))
            .send()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_server_with_no_shutdown_signal_keeps_running() {
    let pki = pki();
    let port = serve(&pki).await;
    let client = client_trusting(&pki);
    for _ in 0..3 {
        assert_eq!(
            client
                .get(format!("https://localhost:{port}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
}

#[tokio::test]
async fn a_request_that_will_not_finish_cannot_hold_shutdown_past_the_grace_period() {
    let pki = pki();
    let config = tls::server_config(&pki.certificate, &pki.key).expect("valid pair");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let slow = app().route(
        "/slow",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            "late"
        }),
    );
    let server = tokio::spawn(async move {
        tls::serve_tls_until(
            listener,
            slow,
            config,
            async {
                let _ = stopped.await;
            },
            Duration::from_secs(1),
        )
        .await
    });
    let client = client_trusting(&pki);
    let url = format!("https://localhost:{port}/slow");
    let in_flight = tokio::spawn(async move { client.get(url).send().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = std::time::Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("a stuck request must not keep the service from stopping")
        .unwrap()
        .unwrap();
    // The grace period is one second; the request itself would have taken sixty.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    in_flight.abort();
}
