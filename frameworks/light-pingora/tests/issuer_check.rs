//! The Gateway takes a client certificate's issuer digest from the second certificate the client
//! sends, and only if that certificate really signed the first. A client chooses the order and
//! contents of what it sends, so the check is what stops a leaf from one trusted CA carrying an
//! unrelated trusted CA's certificate to satisfy that CA's `caTrust.issuerSha256`.

use pingora::utils::tls::{is_issued_by, issuer_in_chain};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

struct Ca {
    der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

fn ca(common_name: &str) -> Ca {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    let cert = params.self_signed(&key).unwrap();
    Ca {
        der: cert.der().to_vec(),
        issuer: Issuer::new(params, key),
    }
}

fn leaf_from(ca: &Ca) -> Vec<u8> {
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["client.example.test".to_string()]).unwrap();
    params.signed_by(&key, &ca.issuer).unwrap().der().to_vec()
}

#[test]
fn only_the_certificate_that_signed_the_leaf_counts_as_its_issuer() {
    let (a, b) = (ca("CA A"), ca("CA B"));
    let leaf_from_b = leaf_from(&b);

    assert!(is_issued_by(&leaf_from_b, &b.der), "the real issuer");
    assert!(
        !is_issued_by(&leaf_from_b, &a.der),
        "another trusted CA sent in its place"
    );
    assert!(
        !is_issued_by(&leaf_from_b, &leaf_from_b),
        "the leaf is not its own issuer"
    );
}

#[test]
fn a_lookalike_with_the_same_name_but_another_key_is_not_the_issuer() {
    let real = ca("Shared CA Name");
    let impostor = ca("Shared CA Name");
    let leaf = leaf_from(&real);

    assert!(is_issued_by(&leaf, &real.der));
    assert!(
        !is_issued_by(&leaf, &impostor.der),
        "names match but the signature does not verify"
    );
}

#[test]
fn anything_that_is_not_a_certificate_is_not_an_issuer() {
    let a = ca("CA A");
    let leaf = leaf_from(&a);
    assert!(!is_issued_by(&leaf, b"not a certificate"));
    assert!(!is_issued_by(b"not a certificate", &a.der));
    assert!(!is_issued_by(&[], &[]));
}

#[test]
fn a_chain_yields_an_issuer_only_when_its_second_entry_signed_its_first() {
    let (a, b) = (ca("CA A"), ca("CA B"));
    let leaf = leaf_from(&b);

    assert_eq!(
        issuer_in_chain(&[leaf.clone(), b.der.clone()]),
        Some(&b.der)
    );
    // Sent in another CA's company, or without the CA, or in the wrong order: no issuer.
    assert_eq!(issuer_in_chain(&[leaf.clone(), a.der.clone()]), None);
    assert_eq!(issuer_in_chain(&[leaf.clone()]), None);
    assert_eq!(issuer_in_chain(&[b.der.clone(), leaf.clone()]), None);
    assert_eq!(issuer_in_chain::<Vec<u8>>(&[]), None);
    // A third certificate later in the chain does not stand in for the second.
    assert_eq!(issuer_in_chain(&[leaf, a.der, b.der]), None);
}
