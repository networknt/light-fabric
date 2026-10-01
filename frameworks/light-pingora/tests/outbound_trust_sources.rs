use pingora::connectors::outbound_trust::{resolve, resolve_legacy, resolve_with_platform_paths};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::{fs, path::Path};

fn certificate() -> String {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .self_signed(&KeyPair::generate().unwrap())
        .unwrap()
        .pem()
}

fn configured_error(contents: &str) -> String {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ca.pem");
    fs::write(&path, contents).unwrap();
    resolve("configured-only", path.to_str())
        .unwrap_err()
        .to_string()
}

fn write_hash(dir: &Path, name: &str, contents: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), contents).unwrap();
}

#[test]
fn pem_accepts_comments_and_descriptive_text() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ca.pem");
    let contents = format!(
        "# Approved enterprise roots\nBag Attributes\nsubject=Example CA\n{}\nCertificate supplied by owner; rotation record follows.\n",
        certificate()
    );
    fs::write(&path, &contents).unwrap();
    let trust = resolve("configured-only", path.to_str()).unwrap();
    assert_eq!(trust.roots.len(), 1);
    assert_eq!(
        trust.configured_content.as_deref(),
        Some(contents.as_bytes())
    );
}

#[test]
fn pem_valid_plus_malformed_block_fails() {
    let contents = format!(
        "{}-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
        certificate()
    );
    assert!(configured_error(&contents).contains("malformed"));
}

#[test]
fn pem_valid_plus_truncated_block_fails() {
    let contents = format!("{}-----BEGIN CERTIFICATE-----\nAA==\n", certificate());
    assert!(configured_error(&contents).contains("incomplete"));
}

#[test]
fn pem_nested_or_unmatched_boundaries_fail() {
    for suffix in [
        "-----END CERTIFICATE-----\n",
        "-----BEGIN CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
        "-----BEGIN CERTIFICATE----\nAA==\n",
        "----BEGIN CERTIFICATE----\nAA==\n----END CERTIFICATE----\n",
        "-----begin certificate-----\nAA==\n-----end certificate-----\n",
    ] {
        assert!(configured_error(&format!("{}{}", certificate(), suffix)).contains("malformed"));
    }
}

#[test]
fn pem_valid_plus_invalid_der_fails() {
    let contents = format!(
        "{}-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
        certificate()
    );
    assert!(configured_error(&contents).contains("DER"));
}

#[test]
fn pem_empty_or_comment_only_fails() {
    for contents in ["", " \n", "# No certificates\nDescriptive text only\n"] {
        assert!(configured_error(contents).contains("empty"));
    }
}

#[test]
fn pem_unsupported_blocks_and_private_keys_fail() {
    let key = KeyPair::generate().unwrap().serialize_pem();
    for unsupported in [
        key.as_str(),
        "-----BEGIN PUBLIC KEY-----\nAA==\n-----END PUBLIC KEY-----\n",
        "-----BEGIN X509 CRL-----\nAA==\n-----END X509 CRL-----\n",
    ] {
        for contents in [
            format!("{}{}", certificate(), unsupported),
            format!("{}{}", unsupported, certificate()),
        ] {
            assert!(configured_error(&contents).contains("only CERTIFICATE"));
        }
    }
}

#[test]
fn pem_trusted_certificate_is_explicitly_rejected() {
    let trusted = certificate()
        .replace("BEGIN CERTIFICATE", "BEGIN TRUSTED CERTIFICATE")
        .replace("END CERTIFICATE", "END TRUSTED CERTIFICATE");
    let error = configured_error(&format!("{}{}", certificate(), trusted));
    assert!(error.contains("TRUSTED CERTIFICATE"));
    assert!(error.contains("trust/rejection attributes"));
}

#[test]
fn native_selected_union_deduplicates_with_deterministic_digest() {
    let temp = tempfile::tempdir().unwrap();
    let a = certificate();
    let b = certificate();
    let file = temp.path().join("native.pem");
    let dir = temp.path().join("native-hashes");
    fs::write(&file, &a).unwrap();
    write_hash(&dir, "01234567.0", &a);
    write_hash(&dir, "abcdefab.1", &b);
    let first = resolve_with_platform_paths("platform", None, Some(&file), Some(&dir)).unwrap();
    assert_eq!(first.platform_source, "native");
    assert_eq!(first.platform_count, 2);
    assert_eq!(first.roots.len(), 2);
    fs::write(&file, format!("# Roots in reverse order\n{}{}", b, a)).unwrap();
    let second = resolve_with_platform_paths("platform", None, Some(&file), Some(&dir)).unwrap();
    assert_eq!(first.digest, second.digest);
    let configured = temp.path().join("private.pem");
    fs::write(&configured, certificate()).unwrap();
    let combined = resolve_with_platform_paths(
        "platform-plus-configured",
        configured.to_str(),
        Some(&file),
        Some(&dir),
    )
    .unwrap();
    assert_eq!(combined.platform_count, 2);
    assert_eq!(combined.configured_count, 1);
    assert_eq!(combined.roots.len(), 3);
}

#[test]
fn native_bad_selected_file_cannot_be_masked_by_valid_directory() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("hashes");
    write_hash(&dir, "01234567.0", &certificate());
    let file = temp.path().join("native.pem");
    let missing = temp.path().join("missing.pem");
    for mode in ["platform", "platform-plus-configured"] {
        let configured = temp.path().join("private.pem");
        fs::write(&configured, certificate()).unwrap();
        for selected in [&missing, &dir] {
            // A directory as a file guarantees a read error, even as root.
            assert!(
                resolve_with_platform_paths(mode, configured.to_str(), Some(selected), Some(&dir))
                    .is_err()
            );
        }
        for contents in [
            "".to_string(),
            "# Empty roots".to_string(),
            "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_string(),
            format!(
                "{}-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
                certificate()
            ),
        ] {
            fs::write(&file, contents).unwrap();
            assert!(
                resolve_with_platform_paths(mode, configured.to_str(), Some(&file), Some(&dir))
                    .is_err()
            );
        }
    }
}

#[test]
fn native_bad_selected_directory_cannot_be_masked_by_valid_file() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("native.pem");
    fs::write(&file, certificate()).unwrap();
    let missing = temp.path().join("missing-dir");
    let empty = temp.path().join("empty-dir");
    fs::create_dir(&empty).unwrap();
    for dir in [&missing, &empty, &file] {
        assert!(resolve_with_platform_paths("platform", None, Some(&file), Some(dir)).is_err());
    }
    let partial = temp.path().join("partial-dir");
    write_hash(&partial, "01234567.0", &certificate());
    write_hash(
        &partial,
        "abcdefab.0",
        "-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
    );
    assert!(resolve_with_platform_paths("platform", None, Some(&file), Some(&partial)).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn native_selected_dangling_hash_fails_without_changing_host_store() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("native.pem");
    fs::write(&file, certificate()).unwrap();
    let dir = temp.path().join("hashes");
    write_hash(&dir, "01234567.0", &certificate());
    std::os::unix::fs::symlink(temp.path().join("missing.pem"), dir.join("abcdefab.0")).unwrap();
    assert!(resolve_with_platform_paths("platform", None, Some(&file), Some(&dir)).is_err());
}

#[test]
fn native_absent_optional_candidates_are_not_selected_failures() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("native.pem");
    fs::write(&file, certificate()).unwrap();
    let dir = temp.path().join("hashes");
    write_hash(&dir, "01234567.0", &certificate());
    // Non-hash candidates are not selected certificate files.
    fs::write(dir.join("unselected.txt"), "-----BEGIN PRIVATE KEY-----").unwrap();
    assert!(resolve_with_platform_paths("platform", None, Some(&file), None).is_ok());
    assert!(resolve_with_platform_paths("platform", None, None, Some(&dir)).is_ok());
    assert!(resolve_with_platform_paths("platform", None, None, None).is_err());
}

#[test]
fn configured_only_does_not_load_injected_native_sources() {
    let temp = tempfile::tempdir().unwrap();
    let configured = temp.path().join("private.pem");
    fs::write(&configured, certificate()).unwrap();
    let missing = temp.path().join("missing");
    let trust = resolve_with_platform_paths(
        "configured-only",
        configured.to_str(),
        Some(&missing),
        Some(&missing),
    )
    .unwrap();
    assert_eq!(trust.platform_source, "none");
    assert_eq!(trust.platform_count, 0);
    assert_eq!(trust.configured_count, 1);
}

#[test]
fn legacy_configured_loading_remains_tolerant() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("legacy.pem");
    fs::write(
        &file,
        format!(
            "Comments before\n{}\nDescriptive text after\n",
            certificate()
        ),
    )
    .unwrap();
    assert_eq!(resolve_legacy(file.to_str()).unwrap().roots.len(), 1);
}
