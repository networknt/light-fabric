use pingora::{
    connectors::{ConnectorOptions, TransportConnector},
    upstreams::peer::{HttpPeer, Peer},
    utils::tls::CertKey,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use std::{fmt, sync::Arc, time::Duration};

#[derive(Clone)]
struct Ordinary(HttpPeer);
impl fmt::Display for Ordinary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Peer for Ordinary {
    fn address(&self) -> &pingora::protocols::l4::socket::SocketAddr {
        self.0.address()
    }
    fn tls(&self) -> bool {
        true
    }
    fn sni(&self) -> &str {
        self.0.sni()
    }
    fn reuse_hash(&self) -> u64 {
        self.0.reuse_hash()
    }
    fn verify_cert(&self) -> bool {
        self.0.verify_cert()
    }
    fn verify_hostname(&self) -> bool {
        self.0.verify_hostname()
    }
}

#[tokio::test]
async fn outbound_trust_real_connector_identity_and_mtls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    let mut ca = CertificateParams::default();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "service-name.example.test".into(),
    ])
    .unwrap()
    .signed_by(&leaf_key, &issuer)
    .unwrap();
    let ca_path = temp.path().join("ca.pem");
    std::fs::write(&ca_path, ca_cert.pem()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca_cert.der().clone()).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .unwrap();
    let mut server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec(), b"h2".to_vec()];
    let server_config = Arc::new(server_config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        while !server_stop.load(std::sync::atomic::Ordering::Relaxed) {
            let mut tcp = match listener.accept() {
                Ok((tcp, _)) => tcp,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("accept: {e}"),
            };
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut tls = rustls::ServerConnection::new(server_config.clone()).unwrap();
            while tls.is_handshaking() {
                if tls.complete_io(&mut tcp).is_err() {
                    break;
                }
            }
        }
    });
    let mut client = light_client::ClientConfig::default();
    client.tls.trust_mode = Some(light_client::OutboundTrustMode::ConfiguredOnly);
    client.tls.ca_cert_path = Some(ca_path.clone());
    let runtime = light_runtime::RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: Some(client),
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: temp.path().into(),
        external_config_dir: temp.path().into(),
        resolved_values: Default::default(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Arc::new(light_runtime::ModuleRegistry::new()),
        cache_registry: None,
        registry_client: None,
    };
    let startup = Arc::new(light_pingora::OutboundTrustSnapshot::capture(&runtime).unwrap());
    let guards = [startup.clone(), startup.clone(), startup.clone()];
    let mut server_conf = pingora::server::configuration::ServerConf::default();
    startup.configure_server(&mut server_conf);
    let options = ConnectorOptions::from_server_conf(&server_conf);
    assert!(Arc::ptr_eq(
        server_conf.resolved_outbound_trust.as_ref().unwrap(),
        options.resolved_outbound_trust.as_ref().unwrap()
    ));
    // Deterministically replace A with B AFTER guard capture but BEFORE connector construction.
    let mut replacement = CertificateParams::default();
    replacement.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let replacement = replacement
        .self_signed(&KeyPair::generate().unwrap())
        .unwrap();
    std::fs::write(&ca_path, replacement.pem()).unwrap();
    let connector = TransportConnector::new(Some(options));
    for guard in &guards {
        assert!(
            guard
                .validate_reload(&runtime)
                .unwrap_err()
                .to_string()
                .contains("restart required")
        );
    }
    let peer = HttpPeer::new(addr, true, "localhost".into());
    assert!(
        connector.new_stream(&peer).await.is_ok(),
        "startup connector must retain resolved A, not read B"
    );
    std::fs::write(&ca_path, ca_cert.pem()).unwrap();
    for guard in &guards {
        guard.validate_reload(&runtime).unwrap();
    }
    let mut h2_peer = HttpPeer::new(addr, true, "localhost".into());
    h2_peer.options.set_http_version(2, 2);
    assert!(connector.new_stream(&h2_peer).await.is_ok());
    h2_peer.sni = "wrong.invalid".into();
    assert!(connector.new_stream(&h2_peer).await.is_err());
    h2_peer.sni = "service_name.example.test".into();
    assert!(connector.new_stream(&h2_peer).await.is_err());
    for identity in ["localhost", "127.0.0.1", "service-name.example.test"] {
        let peer = HttpPeer::new(addr, true, identity.into());
        assert!(
            connector.new_stream(&Ordinary(peer.clone())).await.is_ok(),
            "ordinary {identity}"
        );
        assert!(connector.new_stream(&peer).await.is_ok(), "ALPN {identity}");
    }
    for identity in [
        "wrong.invalid",
        "127.0.0.2",
        "",
        "service_name.example.test",
    ] {
        let peer = HttpPeer::new(addr, true, identity.into());
        assert!(connector.new_stream(&Ordinary(peer.clone())).await.is_err());
        assert!(connector.new_stream(&peer).await.is_err());
    }
    for flag in ["cert", "hostname"] {
        let mut peer = HttpPeer::new(addr, true, "localhost".into());
        if flag == "cert" {
            peer.options.verify_cert = false;
        } else {
            peer.options.verify_hostname = false;
        }
        assert!(connector.new_stream(&Ordinary(peer.clone())).await.is_err());
        assert!(connector.new_stream(&peer).await.is_err());
    }
    let client_key = KeyPair::generate().unwrap();
    let client = CertificateParams::new(vec!["client".into()])
        .unwrap()
        .signed_by(&client_key, &issuer)
        .unwrap();
    let cert_key = Arc::new(CertKey::new(
        vec![client.der().to_vec()],
        client_key.serialize_der(),
    ));
    let peer = HttpPeer::new_mtls(addr, "localhost".into(), cert_key.clone());
    assert!(connector.new_stream(&peer).await.is_ok());
    for identity in ["wrong.invalid", "", "service_name.example.test"] {
        let peer = HttpPeer::new_mtls(addr, identity.into(), cert_key.clone());
        assert!(connector.new_stream(&peer).await.is_err());
    }
    // Legacy ALPN/mTLS still retain the historical underscore rewrite.
    let mut legacy_options = ConnectorOptions::new(0);
    legacy_options.ca_file = Some(ca_path.to_string_lossy().into());
    let legacy = TransportConnector::new(Some(legacy_options));
    let peer = HttpPeer::new(addr, true, "service_name.example.test".into());
    assert!(legacy.new_stream(&peer).await.is_ok());
    let peer = HttpPeer::new_mtls(addr, "service_name.example.test".into(), cert_key.clone());
    assert!(legacy.new_stream(&peer).await.is_ok());
    for flag in ["cert", "hostname"] {
        let mut peer = HttpPeer::new_mtls(addr, "localhost".into(), cert_key.clone());
        if flag == "cert" {
            peer.options.verify_cert = false;
        } else {
            peer.options.verify_hostname = false;
        }
        assert!(connector.new_stream(&peer).await.is_err());
    }
    // Existing connector retains startup roots; a restart reads the changed file.
    let other_key = KeyPair::generate().unwrap();
    let mut other = CertificateParams::default();
    other.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let other = other.self_signed(&other_key).unwrap();
    std::fs::write(&ca_path, other.pem()).unwrap();
    let peer = HttpPeer::new(addr, true, "localhost".into());
    assert!(connector.new_stream(&peer).await.is_ok());
    let mut restarted_options = ConnectorOptions::new(0);
    restarted_options.ca_file = Some(ca_path.to_string_lossy().into());
    restarted_options.outbound_trust_mode = Some("configured-only".into());
    let restarted = TransportConnector::new(Some(restarted_options));
    assert!(restarted.new_stream(&peer).await.is_err());
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}
