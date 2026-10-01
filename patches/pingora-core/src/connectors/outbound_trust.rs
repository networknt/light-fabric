//! Explicit outbound root composition. Legacy loading stays in the connector.
use pingora_error::{Error, ErrorType::InvalidCert, Result};
use pingora_rustls::{CertificateDer, RootCertStore};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, io::Cursor, path::Path};

#[derive(Clone, Debug)]
pub struct ResolvedTrust {
    pub roots: RootCertStore,
    pub digest: String,
    pub platform_count: usize,
    pub configured_count: usize,
    pub platform_source: &'static str,
    pub configured_content: Option<Vec<u8>>,
}

impl PartialEq for ResolvedTrust {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
            && self.platform_source == other.platform_source
            && self.configured_content == other.configured_content
    }
}
impl Eq for ResolvedTrust {}

fn invalid(message: &str) -> Box<Error> {
    Error::explain(InvalidCert, message.to_owned())
}

// Native loader 0.7 accepts partial success and silently ignores non-PEM data.
// Allow descriptive text outside blocks, but validate every PEM block explicitly.
fn strict_file(path: &Path) -> Result<(Vec<CertificateDer<'static>>, Vec<u8>)> {
    let bytes = fs::read(path).map_err(|_| invalid("outbound CA source is unreadable"))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("outbound CA source is not PEM"))?;
    let mut rest = text;
    let mut certs = Vec::new();
    loop {
        // Recognize malformed dash-delimited boundaries too, so descriptive
        // text tolerance cannot hide a damaged PEM block beside a valid one.
        let upper = rest.to_ascii_uppercase();
        let next = ["BEGIN", "END"]
            .into_iter()
            .flat_map(|marker| upper.match_indices(marker))
            .filter_map(|(index, _)| {
                let dashes = rest[..index]
                    .bytes()
                    .rev()
                    .take_while(|b| *b == b'-')
                    .count();
                (dashes != 0).then_some(index - dashes)
            })
            .min();
        let Some(next) = next else {
            break;
        };
        rest = &rest[next..];
        if rest.starts_with("-----BEGIN TRUSTED CERTIFICATE") {
            return Err(invalid(
                "TRUSTED CERTIFICATE bundles are unsupported: trust/rejection attributes cannot be preserved; use approved CERTIFICATE blocks",
            ));
        }
        if !rest.starts_with("-----BEGIN CERTIFICATE-----") {
            return Err(invalid(
                "unsupported or malformed PEM block in outbound CA source; only CERTIFICATE blocks are supported (no private keys)",
            ));
        }
        let end = rest
            .find("-----END CERTIFICATE-----")
            .ok_or_else(|| invalid("incomplete outbound CA certificate"))?
            + "-----END CERTIFICATE-----".len();
        let block = &rest[..end];
        let body = &block
            ["-----BEGIN CERTIFICATE-----".len()..block.len() - "-----END CERTIFICATE-----".len()];
        if body.contains("-----BEGIN") || body.contains("-----END") {
            return Err(invalid("malformed nested PEM block in outbound CA source"));
        }
        let parsed = rustls_pemfile::certs(&mut Cursor::new(block.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| invalid("malformed outbound CA certificate"))?;
        if parsed.len() != 1 {
            return Err(invalid("invalid outbound CA certificate"));
        }
        // Validate DER even when another source already supplies trusted anchors.
        let mut store = RootCertStore::empty();
        store
            .add(parsed[0].clone())
            .map_err(|_| invalid("invalid outbound CA certificate DER"))?;
        certs.extend(parsed);
        rest = &rest[end..];
    }
    if certs.is_empty() {
        return Err(invalid("outbound CA source is empty"));
    }
    Ok((certs, bytes))
}

fn platform() -> Result<(Vec<CertificateDer<'static>>, &'static str)> {
    let file = std::env::var_os("SSL_CERT_FILE");
    let dir = std::env::var_os("SSL_CERT_DIR");
    if file.is_none() && dir.is_none() {
        #[cfg(target_os = "linux")]
        {
            let selected = openssl_probe::probe();
            return load_selected_platform_sources(
                selected.cert_file.as_deref(),
                selected.cert_dir.as_deref(),
            )
            .map(|certs| (certs, "native"));
        }
        // Non-Linux native behavior is outside v1 qualification.
        #[cfg(not(target_os = "linux"))]
        return pingora_rustls::load_native_certs()
            .map(|certs| (certs, "native"))
            .map_err(|_| invalid("failed to load platform roots"));
    }
    // Explicit overrides replace discovery, with FILE + hash-named DIR union.
    let certs = load_selected_platform_sources(
        file.as_deref().map(Path::new),
        dir.as_deref().map(Path::new),
    )?;
    let origin = match (file.is_some(), dir.is_some()) {
        (true, true) => "SSL_CERT_FILE+SSL_CERT_DIR",
        (true, false) => "SSL_CERT_FILE",
        _ => "SSL_CERT_DIR",
    };
    Ok((certs, origin))
}

// None means discovery did not select this optional candidate. Some means the
// selected source must load successfully even when another source is usable.
fn load_selected_platform_sources(
    file: Option<&Path>,
    dir: Option<&Path>,
) -> Result<Vec<CertificateDer<'static>>> {
    let mut certs = Vec::new();
    if let Some(file) = file {
        certs.extend(strict_file(file)?.0);
    }
    if let Some(dir) = dir {
        let mut count = 0;
        for entry in fs::read_dir(dir)
            .map_err(|_| invalid("selected platform CA directory is unreadable"))?
        {
            let entry =
                entry.map_err(|_| invalid("selected platform CA directory entry is unreadable"))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let b = name.as_bytes();
            if b.len() == 10
                && b[..8].iter().all(u8::is_ascii_hexdigit)
                && b[8] == b'.'
                && b[9].is_ascii_digit()
            {
                certs.extend(strict_file(&entry.path())?.0);
                count += 1;
            }
        }
        if count == 0 {
            return Err(invalid(
                "selected platform CA directory has no certificates",
            ));
        }
    }
    Ok(certs)
}

pub fn resolve(mode: &str, ca_file: Option<&str>) -> Result<ResolvedTrust> {
    resolve_with_platform(mode, ca_file, platform)
}

/// Deterministic boundary for qualifying Linux discovered sources without
/// modifying the system store or setting SSL_CERT_FILE/SSL_CERT_DIR.
#[doc(hidden)]
pub fn resolve_with_platform_paths(
    mode: &str,
    ca_file: Option<&str>,
    file: Option<&Path>,
    dir: Option<&Path>,
) -> Result<ResolvedTrust> {
    resolve_with_platform(mode, ca_file, || {
        load_selected_platform_sources(file, dir).map(|certs| (certs, "native"))
    })
}

fn resolve_with_platform(
    mode: &str,
    ca_file: Option<&str>,
    load_platform: impl FnOnce() -> Result<(Vec<CertificateDer<'static>>, &'static str)>,
) -> Result<ResolvedTrust> {
    let use_platform = match mode {
        "platform" | "platform-plus-configured" => true,
        "configured-only" => false,
        _ => return Err(invalid("unknown outbound trust mode")),
    };
    let (platform_certs, origin) = if use_platform {
        load_platform()?
    } else {
        (Vec::new(), "none")
    };
    if use_platform && platform_certs.is_empty() {
        return Err(invalid("platform root store is empty"));
    }
    let (configured, configured_content) = if mode != "platform" {
        let (certs, bytes) = strict_file(Path::new(
            ca_file
                .filter(|p| !p.is_empty())
                .ok_or_else(|| invalid("explicit configured CA bundle required"))?,
        ))?;
        (certs, Some(bytes))
    } else {
        (Vec::new(), None)
    };
    let mut roots = RootCertStore::empty();
    let mut fingerprints = BTreeSet::new();
    let mut counts = [0usize; 2];
    for (i, certs) in [platform_certs, configured].into_iter().enumerate() {
        for cert in certs {
            let fingerprint = format!("{:x}", Sha256::digest(cert.as_ref()));
            if fingerprints.insert(fingerprint) {
                roots
                    .add(cert)
                    .map_err(|_| invalid("invalid outbound root certificate"))?;
                counts[i] += 1;
            }
        }
    }
    let mut hash = Sha256::new();
    hash.update(mode.as_bytes());
    hash.update([0]);
    for fingerprint in fingerprints {
        hash.update(fingerprint.as_bytes());
        hash.update([0]);
    }
    Ok(ResolvedTrust {
        roots,
        digest: format!("sha256:{:x}", hash.finalize()),
        platform_count: counts[0],
        configured_count: counts[1],
        platform_source: origin,
        configured_content,
    })
}

/// Legacy loaders retain their existing permissive source-selection behavior.
pub fn resolve_legacy(ca_file: Option<&str>) -> Result<ResolvedTrust> {
    let mut roots = RootCertStore::empty();
    if let Some(path) = ca_file {
        pingora_rustls::load_ca_file_into_store(path, &mut roots)?;
    } else {
        pingora_rustls::load_platform_certs_incl_env_into_store(&mut roots)?;
    }
    // Hash the actual loaded anchors, including their constraints, not a second read.
    let mut anchors: Vec<_> = roots
        .roots
        .iter()
        .map(|root| format!("{:?}", root))
        .collect();
    anchors.sort();
    let digest = format!("sha256:{:x}", Sha256::digest(anchors.join("\0").as_bytes()));
    Ok(ResolvedTrust {
        platform_count: if ca_file.is_none() { roots.len() } else { 0 },
        configured_count: if ca_file.is_some() { roots.len() } else { 0 },
        roots,
        digest,
        platform_source: "legacy",
        configured_content: None,
    })
}
