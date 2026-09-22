// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! TLS information from the TLS connection

use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

/// The TLS connection information
#[derive(Clone, Debug)]
pub struct SslDigest {
    /// The cipher used
    pub cipher: Cow<'static, str>,
    /// The TLS version of this connection
    pub version: Cow<'static, str>,
    /// The organization of the peer's certificate
    pub organization: Option<String>,
    /// The serial number of the peer's certificate
    pub serial_number: Option<String>,
    /// The digest of the peer's certificate
    pub cert_digest: Vec<u8>,
    /// The digest of the certificate that issued the peer's certificate: the next entry in the
    /// presented chain, when the peer sent one **and** it verifiably signed the peer's certificate.
    /// Used by CA-based peer trust (matching "chains to this CA" rather
    /// than an exact leaf fingerprint) instead of platform-specific
    /// attestation.
    pub issuer_digest: Option<Vec<u8>>,
    /// The first `URI` SAN on the peer's certificate, if any. Used to carry
    /// a `spiffe://`-shaped workload identity attribute for CA-based peer
    /// trust.
    pub uri_san: Option<String>,
    /// The user-defined TLS data
    pub extension: SslDigestExtension,
}

impl SslDigest {
    /// Create a new SslDigest
    pub fn new<S>(
        cipher: S,
        version: S,
        organization: Option<String>,
        serial_number: Option<String>,
        cert_digest: Vec<u8>,
    ) -> Self
    where
        S: Into<Cow<'static, str>>,
    {
        Self::new_with_chain_attributes(
            cipher,
            version,
            organization,
            serial_number,
            cert_digest,
            None,
            None,
        )
    }

    /// Create a new SslDigest carrying the issuer digest and URI SAN
    /// extracted from the peer's certificate chain, for backends that
    /// support it.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_chain_attributes<S>(
        cipher: S,
        version: S,
        organization: Option<String>,
        serial_number: Option<String>,
        cert_digest: Vec<u8>,
        issuer_digest: Option<Vec<u8>>,
        uri_san: Option<String>,
    ) -> Self
    where
        S: Into<Cow<'static, str>>,
    {
        SslDigest {
            cipher: cipher.into(),
            version: version.into(),
            organization,
            serial_number,
            cert_digest,
            issuer_digest,
            uri_san,
            extension: SslDigestExtension::default(),
        }
    }
}

/// The user-defined TLS data
#[derive(Clone, Debug, Default)]
pub struct SslDigestExtension {
    value: Option<Arc<dyn Any + Send + Sync>>,
}

impl SslDigestExtension {
    /// Retrieves a reference to the user-defined TLS data if it matches the specified type.
    ///
    /// Returns `None` if no data has been set or if the data is not of type `T`.
    pub fn get<T>(&self) -> Option<&T>
    where
        T: Send + Sync + 'static,
    {
        self.value.as_ref().and_then(|v| v.downcast_ref::<T>())
    }

    #[allow(dead_code)]
    pub(crate) fn set(&mut self, value: Arc<dyn Any + Send + Sync>) {
        self.value = Some(value);
    }
}
