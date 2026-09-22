//! Background-refreshed JWKS cache backing `PortalTokenAuthorizer`'s key
//! resolution. Kept separate from the synchronous `DecodingKeyResolver`
//! trait in `light-identity-issuer` so verification itself never blocks on
//! a network call: this task refreshes an in-memory map, and `resolve()`
//! only ever reads it.
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jsonwebtoken::{DecodingKey, jwk::JwkSet};
use light_identity_issuer::DecodingKeyResolver;
use tracing::warn;

const JWKS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct JwksCache {
    client: reqwest::Client,
    keys: RwLock<HashMap<String, DecodingKey>>,
}

impl JwksCache {
    fn new(client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            client,
            keys: RwLock::new(HashMap::new()),
        })
    }

    /// An empty, never-refreshed cache. For tests exercising only the
    /// pairing-grant path, where no Portal token is ever presented.
    #[doc(hidden)]
    pub fn empty_for_tests() -> Arc<Self> {
        Self::new(reqwest::Client::new())
    }

    /// Install a decoding key directly, so a test can mint tokens the cache
    /// will accept without a live `light-oauth`.
    #[doc(hidden)]
    pub fn insert_for_tests(&self, kid: &str, key: DecodingKey) {
        self.keys
            .write()
            .expect("jwks cache lock")
            .insert(kid.to_string(), key);
    }

    async fn refresh_once(&self, url: &str) -> Result<(), String> {
        let body = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|err| err.to_string())?
            .error_for_status()
            .map_err(|err| err.to_string())?
            .text()
            .await
            .map_err(|err| err.to_string())?;
        let resolved = decode_keys(&body)?;
        *self.keys.write().expect("jwks cache lock") = resolved;
        Ok(())
    }

    /// Fetch once synchronously (so the service fails fast at startup if
    /// `light-oauth`'s JWKS endpoint is unreachable), then keep refreshing
    /// in the background on `interval`.
    pub async fn spawn(
        url: String,
        interval: Duration,
        extra_root_cert_pem: Option<&str>,
    ) -> Result<Arc<Self>, String> {
        let mut builder = reqwest::Client::builder().timeout(JWKS_REQUEST_TIMEOUT);
        if let Some(pem) = extra_root_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|err| format!("invalid jwks_ca_cert_path PEM: {err}"))?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder
            .build()
            .map_err(|err| format!("could not build JWKS HTTP client: {err}"))?;
        let cache = Self::new(client);
        cache.refresh_once(&url).await?;
        let background = Arc::clone(&cache);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(err) = background.refresh_once(&url).await {
                    warn!(%err, "JWKS refresh failed, keeping previous keys");
                }
            }
        });
        Ok(cache)
    }
}

fn decode_keys(body: &str) -> Result<HashMap<String, DecodingKey>, String> {
    let jwk_set: JwkSet = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let mut resolved = HashMap::new();
    for jwk in &jwk_set.keys {
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        match DecodingKey::from_jwk(jwk) {
            Ok(key) => {
                resolved.insert(kid, key);
            }
            Err(err) => warn!(kid, %err, "skipping unsupported JWK"),
        }
    }
    if resolved.is_empty() {
        return Err("JWKS response contained no usable keys".into());
    }
    Ok(resolved)
}

impl DecodingKeyResolver for JwksCache {
    fn resolve(&self, kid: &str) -> Option<DecodingKey> {
        self.keys.read().expect("jwks cache lock").get(kid).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_unusable_jwks_is_not_a_valid_refresh() {
        for body in [r#"{"keys":[]}"#, r#"{"keys":[{"kty":"oct"}]}"#] {
            assert!(decode_keys(body).is_err(), "{body}");
        }
    }
}
