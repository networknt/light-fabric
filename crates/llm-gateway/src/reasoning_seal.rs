use crate::config::{
    ReasoningSealConfig, ReasoningSealKeyReference, ReasoningSealState, ReasoningStateLimits,
};
use crate::credentials::SecretResolver;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use model_provider::inference::ClientProtocol;
use ring::{aead, rand as ring_rand};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use thiserror::Error;
use zeroize::Zeroizing;

const ENVELOPE_VERSION: u8 = 1;
const NONCE_BYTES: usize = 12;
const ROUTE_BINDING_BYTES: usize = 16;
const MAX_ENVELOPE_METADATA_BYTES: usize = 2 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningBinding<'a> {
    pub tenant_id: &'a str,
    pub alias: &'a str,
    pub client_protocol: ClientProtocol,
    pub deployment_id: &'a str,
    pub provider_material_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCandidate<'a> {
    pub deployment_id: &'a str,
    pub provider_material_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReasoningState {
    pub deployment_id: String,
    pub provider_state: Zeroizing<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReasoningSealError {
    #[error("reasoning_state_disabled")]
    Disabled,
    #[error("reasoning_state_too_large")]
    TooLarge,
    #[error("reasoning_state_too_many_items")]
    TooManyItems,
    #[error("reasoning_state_invalid")]
    Invalid,
    #[error("reasoning_state_required")]
    Required,
    #[error("reasoning_state_tampered")]
    Tampered,
    #[error("reasoning_state_stale")]
    Stale,
    #[error("reasoning_route_unavailable")]
    RouteUnavailable,
    #[error("reasoning_key_unavailable")]
    KeyUnavailable,
    #[error("reasoning_key_config_invalid")]
    KeyConfigInvalid,
}

impl ReasoningSealError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Disabled => "reasoning_state_disabled",
            Self::TooLarge => "reasoning_state_too_large",
            Self::TooManyItems => "reasoning_state_too_many_items",
            Self::Invalid => "reasoning_state_invalid",
            Self::Required => "reasoning_state_required",
            Self::Tampered => "reasoning_state_tampered",
            Self::Stale => "reasoning_state_stale",
            Self::RouteUnavailable => "reasoning_route_unavailable",
            Self::KeyUnavailable => "reasoning_key_unavailable",
            Self::KeyConfigInvalid => "reasoning_key_config_invalid",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    version: u8,
    key_id: String,
    key_set_generation: u64,
    provider_material_generation: u64,
    route_binding: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssociatedData<'a> {
    version: u8,
    key_id: &'a str,
    key_set_generation: u64,
    tenant_id: &'a str,
    alias: &'a str,
    client_protocol: ClientProtocol,
    deployment_id: &'a str,
    provider_material_generation: u64,
}

struct KeyMaterial {
    route_key: Zeroizing<[u8; 32]>,
    sealing_key: aead::LessSafeKey,
}

pub struct ReasoningSealer {
    state: ReasoningSealState,
    key_set_generation: u64,
    current_key_id: Option<String>,
    keys: BTreeMap<String, KeyMaterial>,
    limits: ReasoningStateLimits,
    random: ring_rand::SystemRandom,
}

impl Default for ReasoningSealer {
    fn default() -> Self {
        struct EmptyResolver;
        impl SecretResolver for EmptyResolver {
            fn resolve(&self, _reference: &str) -> Result<String, crate::error::LlmGatewayError> {
                Err(crate::error::LlmGatewayError::Config(
                    "reasoning key unavailable".to_string(),
                ))
            }
        }
        Self::resolve(&ReasoningSealConfig::default(), &EmptyResolver)
            .expect("disabled reasoning seal config is valid")
    }
}

impl ReasoningSealer {
    pub fn resolve(
        config: &ReasoningSealConfig,
        resolver: &dyn SecretResolver,
    ) -> Result<Self, ReasoningSealError> {
        validate_config(config)?;
        let mut keys = BTreeMap::new();
        for reference in [config.current.as_ref(), config.previous.as_ref()]
            .into_iter()
            .flatten()
        {
            let material = resolver
                .resolve(&reference.credential_ref)
                .map_err(|_| ReasoningSealError::KeyUnavailable)?;
            let decoded = Zeroizing::new(
                URL_SAFE_NO_PAD
                    .decode(
                        material
                            .trim()
                            .strip_prefix("base64:")
                            .unwrap_or(material.trim()),
                    )
                    .map_err(|_| ReasoningSealError::KeyConfigInvalid)?,
            );
            let key: [u8; 32] = decoded
                .as_slice()
                .try_into()
                .map_err(|_| ReasoningSealError::KeyConfigInvalid)?;
            let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
                .map_err(|_| ReasoningSealError::KeyConfigInvalid)?;
            keys.insert(
                reference.key_id.clone(),
                KeyMaterial {
                    route_key: Zeroizing::new(key),
                    sealing_key: aead::LessSafeKey::new(unbound),
                },
            );
        }
        Ok(Self {
            state: config.state,
            key_set_generation: config.key_set_generation,
            current_key_id: config.current.as_ref().map(|key| key.key_id.clone()),
            keys,
            limits: config.limits.clone(),
            random: ring_rand::SystemRandom::new(),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.state == ReasoningSealState::Disabled
            || (self.current_key_id.is_some() && !self.keys.is_empty())
    }

    pub fn seal(
        &self,
        binding: &ReasoningBinding<'_>,
        provider_state: &[u8],
    ) -> Result<String, ReasoningSealError> {
        if self.state != ReasoningSealState::Active {
            return Err(ReasoningSealError::Disabled);
        }
        if provider_state.len() > self.limits.max_decoded_provider_state_bytes {
            return Err(ReasoningSealError::TooLarge);
        }
        let key_id = self
            .current_key_id
            .as_deref()
            .ok_or(ReasoningSealError::KeyUnavailable)?;
        let key = self
            .keys
            .get(key_id)
            .ok_or(ReasoningSealError::KeyUnavailable)?;
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        ring_rand::SecureRandom::fill(&self.random, &mut nonce_bytes)
            .map_err(|_| ReasoningSealError::KeyUnavailable)?;
        let aad = associated_data(key_id, self.key_set_generation, binding)?;
        let mut ciphertext = Zeroizing::new(provider_state.to_vec());
        key.sealing_key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce_bytes),
                aead::Aad::from(aad),
                &mut *ciphertext,
            )
            .map_err(|_| ReasoningSealError::KeyUnavailable)?;
        let envelope = Envelope {
            version: ENVELOPE_VERSION,
            key_id: key_id.to_string(),
            key_set_generation: self.key_set_generation,
            provider_material_generation: binding.provider_material_generation,
            route_binding: route_binding(key, binding.deployment_id)?,
            nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext.as_slice()),
        };
        let encoded = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&envelope).map_err(|_| ReasoningSealError::Invalid)?);
        if encoded.len() > self.limits.max_encoded_item_bytes {
            return Err(ReasoningSealError::TooLarge);
        }
        Ok(encoded)
    }

    pub fn unseal(
        &self,
        encoded: &str,
        tenant_id: &str,
        alias: &str,
        client_protocol: ClientProtocol,
        candidates: &[RouteCandidate<'_>],
    ) -> Result<ResolvedReasoningState, ReasoningSealError> {
        let envelope = self.parse_envelope(encoded)?;
        let key = self
            .keys
            .get(&envelope.key_id)
            .ok_or(ReasoningSealError::KeyUnavailable)?;
        let candidate = candidates
            .iter()
            .find(|candidate| {
                route_binding(key, candidate.deployment_id)
                    .is_ok_and(|binding| binding == envelope.route_binding)
            })
            .ok_or(ReasoningSealError::RouteUnavailable)?;
        if candidate.provider_material_generation != envelope.provider_material_generation {
            return Err(ReasoningSealError::Stale);
        }
        let binding = ReasoningBinding {
            tenant_id,
            alias,
            client_protocol,
            deployment_id: candidate.deployment_id,
            provider_material_generation: candidate.provider_material_generation,
        };
        let aad = associated_data(&envelope.key_id, envelope.key_set_generation, &binding)?;
        let nonce: [u8; NONCE_BYTES] = URL_SAFE_NO_PAD
            .decode(&envelope.nonce)
            .map_err(|_| ReasoningSealError::Invalid)?
            .try_into()
            .map_err(|_| ReasoningSealError::Invalid)?;
        let mut ciphertext = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(&envelope.ciphertext)
                .map_err(|_| ReasoningSealError::Invalid)?,
        );
        if ciphertext.len() > self.limits.max_decoded_provider_state_bytes + aead::MAX_TAG_LEN {
            return Err(ReasoningSealError::TooLarge);
        }
        let plaintext = key
            .sealing_key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad),
                &mut ciphertext,
            )
            .map_err(|_| ReasoningSealError::Tampered)?;
        if plaintext.len() > self.limits.max_decoded_provider_state_bytes {
            return Err(ReasoningSealError::TooLarge);
        }
        Ok(ResolvedReasoningState {
            deployment_id: candidate.deployment_id.to_string(),
            provider_state: Zeroizing::new(plaintext.to_vec()),
        })
    }

    pub fn validate_request_items<'a>(
        &self,
        items: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), ReasoningSealError> {
        let mut count = 0usize;
        let mut cumulative = 0usize;
        for item in items {
            count = count
                .checked_add(1)
                .ok_or(ReasoningSealError::TooManyItems)?;
            if count > self.limits.max_items_per_request {
                return Err(ReasoningSealError::TooManyItems);
            }
            if item.len() > self.limits.max_encoded_item_bytes {
                return Err(ReasoningSealError::TooLarge);
            }
            cumulative = cumulative
                .checked_add(item.len())
                .ok_or(ReasoningSealError::TooLarge)?;
            if cumulative > self.limits.max_cumulative_encoded_bytes {
                return Err(ReasoningSealError::TooLarge);
            }
        }
        Ok(())
    }

    pub fn unseal_request_items<'a>(
        &self,
        items: impl IntoIterator<Item = &'a str>,
        tenant_id: &str,
        alias: &str,
        client_protocol: ClientProtocol,
        candidates: &[RouteCandidate<'_>],
    ) -> Result<Vec<ResolvedReasoningState>, ReasoningSealError> {
        let items = items.into_iter().collect::<Vec<_>>();
        self.validate_request_items(items.iter().copied())?;
        let mut cumulative_decoded = 0usize;
        let mut resolved = Vec::with_capacity(items.len());
        for item in items {
            let state = self.unseal(item, tenant_id, alias, client_protocol, candidates)?;
            cumulative_decoded = cumulative_decoded
                .checked_add(state.provider_state.len())
                .ok_or(ReasoningSealError::TooLarge)?;
            if cumulative_decoded > self.limits.max_cumulative_decoded_bytes {
                return Err(ReasoningSealError::TooLarge);
            }
            resolved.push(state);
        }
        Ok(resolved)
    }

    fn parse_envelope(&self, encoded: &str) -> Result<Envelope, ReasoningSealError> {
        self.validate_request_items([encoded])?;
        let estimated_decoded = encoded
            .len()
            .checked_mul(3)
            .and_then(|value| value.checked_div(4))
            .ok_or(ReasoningSealError::TooLarge)?;
        if estimated_decoded
            > self.limits.max_decoded_provider_state_bytes
                + MAX_ENVELOPE_METADATA_BYTES
                + aead::MAX_TAG_LEN
        {
            return Err(ReasoningSealError::TooLarge);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ReasoningSealError::Invalid)?;
        let envelope: Envelope =
            serde_json::from_slice(&decoded).map_err(|_| ReasoningSealError::Invalid)?;
        if envelope.version != ENVELOPE_VERSION
            || envelope.key_id.is_empty()
            || envelope.key_id.len() > 128
            || envelope.key_set_generation == 0
            || envelope.key_set_generation > self.key_set_generation
            || envelope.route_binding.len() > 64
            || envelope.nonce.len() > 64
        {
            return Err(ReasoningSealError::Invalid);
        }
        Ok(envelope)
    }
}

fn validate_config(config: &ReasoningSealConfig) -> Result<(), ReasoningSealError> {
    let limits = &config.limits;
    if limits.max_encoded_item_bytes == 0
        || limits.max_decoded_provider_state_bytes == 0
        || limits.max_items_per_request == 0
        || limits.max_cumulative_encoded_bytes < limits.max_encoded_item_bytes
        || limits.max_cumulative_decoded_bytes < limits.max_decoded_provider_state_bytes
    {
        return Err(ReasoningSealError::KeyConfigInvalid);
    }
    if config.state == ReasoningSealState::Disabled {
        return Ok(());
    }
    if config.key_set_generation == 0 || config.current.is_none() {
        return Err(ReasoningSealError::KeyConfigInvalid);
    }
    let current = config.current.as_ref().expect("checked");
    validate_reference(current)?;
    if let Some(previous) = &config.previous {
        validate_reference(previous)?;
        if previous.key_id == current.key_id {
            return Err(ReasoningSealError::KeyConfigInvalid);
        }
    }
    Ok(())
}

fn validate_reference(reference: &ReasoningSealKeyReference) -> Result<(), ReasoningSealError> {
    if reference.key_id.is_empty()
        || reference.key_id.len() > 128
        || reference.credential_ref.is_empty()
    {
        Err(ReasoningSealError::KeyConfigInvalid)
    } else {
        Ok(())
    }
}

fn associated_data(
    key_id: &str,
    key_set_generation: u64,
    binding: &ReasoningBinding<'_>,
) -> Result<Vec<u8>, ReasoningSealError> {
    serde_json::to_vec(&AssociatedData {
        version: ENVELOPE_VERSION,
        key_id,
        key_set_generation,
        tenant_id: binding.tenant_id,
        alias: binding.alias,
        client_protocol: binding.client_protocol,
        deployment_id: binding.deployment_id,
        provider_material_generation: binding.provider_material_generation,
    })
    .map_err(|_| ReasoningSealError::Invalid)
}

fn route_binding(key: &KeyMaterial, deployment_id: &str) -> Result<String, ReasoningSealError> {
    let mut mac = HmacSha256::new_from_slice(key.route_key.as_ref())
        .map_err(|_| ReasoningSealError::KeyConfigInvalid)?;
    mac.update(b"light-gateway/reasoning-route/v1\0");
    mac.update(deployment_id.as_bytes());
    let digest = mac.finalize().into_bytes();
    Ok(URL_SAFE_NO_PAD.encode(&digest[..ROUTE_BINDING_BYTES]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MapSecretResolver;

    fn config(
        current: (&str, u8),
        previous: Option<(&str, u8)>,
        generation: u64,
    ) -> ReasoningSealConfig {
        ReasoningSealConfig {
            state: ReasoningSealState::Active,
            key_set_generation: generation,
            current: Some(ReasoningSealKeyReference {
                key_id: current.0.to_string(),
                credential_ref: format!("credential://{}", current.0),
            }),
            previous: previous.map(|(key_id, _)| ReasoningSealKeyReference {
                key_id: key_id.to_string(),
                credential_ref: format!("credential://{key_id}"),
            }),
            limits: ReasoningStateLimits::default(),
        }
    }

    fn sealer(config: &ReasoningSealConfig, keys: &[(&str, u8)]) -> ReasoningSealer {
        let values = keys
            .iter()
            .map(|(key_id, byte)| {
                (
                    format!("credential://{key_id}"),
                    URL_SAFE_NO_PAD.encode([*byte; 32]),
                )
            })
            .collect();
        let resolver = MapSecretResolver(values);
        ReasoningSealer::resolve(config, &resolver).expect("sealer")
    }

    fn binding<'a>(
        tenant: &'a str,
        alias: &'a str,
        deployment: &'a str,
        generation: u64,
    ) -> ReasoningBinding<'a> {
        ReasoningBinding {
            tenant_id: tenant,
            alias,
            client_protocol: ClientProtocol::OpenAiResponses,
            deployment_id: deployment,
            provider_material_generation: generation,
        }
    }

    #[test]
    fn cross_replica_round_trip_and_route_pinning() {
        let config = config(("key-2", 2), Some(("key-1", 1)), 2);
        let first = sealer(&config, &[("key-2", 2), ("key-1", 1)]);
        let second = sealer(&config, &[("key-2", 2), ("key-1", 1)]);
        let sealed = first
            .seal(
                &binding("tenant-a", "claude", "deployment-a", 7),
                b"signed-state",
            )
            .expect("seal");
        let resolved = second
            .unseal(
                &sealed,
                "tenant-a",
                "claude",
                ClientProtocol::OpenAiResponses,
                &[
                    RouteCandidate {
                        deployment_id: "deployment-b",
                        provider_material_generation: 3,
                    },
                    RouteCandidate {
                        deployment_id: "deployment-a",
                        provider_material_generation: 7,
                    },
                ],
            )
            .expect("unseal");
        assert_eq!(resolved.deployment_id, "deployment-a");
        assert_eq!(resolved.provider_state.as_slice(), b"signed-state");
    }

    #[test]
    fn rejects_tenant_alias_protocol_and_tamper_replay() {
        let config = config(("key-1", 1), None, 1);
        let sealer = sealer(&config, &[("key-1", 1)]);
        let sealed = sealer
            .seal(&binding("tenant-a", "claude", "deployment-a", 7), b"state")
            .unwrap();
        let candidates = [RouteCandidate {
            deployment_id: "deployment-a",
            provider_material_generation: 7,
        }];
        for (tenant, alias, protocol) in [
            ("tenant-b", "claude", ClientProtocol::OpenAiResponses),
            ("tenant-a", "other", ClientProtocol::OpenAiResponses),
            ("tenant-a", "claude", ClientProtocol::AnthropicMessages),
        ] {
            assert_eq!(
                sealer
                    .unseal(&sealed, tenant, alias, protocol, &candidates)
                    .unwrap_err(),
                ReasoningSealError::Tampered
            );
        }
        let mut tampered = sealed.into_bytes();
        let index = tampered.len() / 2;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(matches!(
            sealer.unseal(
                &tampered,
                "tenant-a",
                "claude",
                ClientProtocol::OpenAiResponses,
                &candidates
            ),
            Err(ReasoningSealError::Invalid
                | ReasoningSealError::Tampered
                | ReasoningSealError::RouteUnavailable)
        ));
    }

    #[test]
    fn material_rotation_is_stale_and_has_no_drop_and_retry_escape() {
        let config = config(("key-1", 1), None, 1);
        let sealer = sealer(&config, &[("key-1", 1)]);
        let sealed = sealer
            .seal(&binding("tenant-a", "claude", "deployment-a", 7), b"state")
            .unwrap();
        let error = sealer
            .unseal(
                &sealed,
                "tenant-a",
                "claude",
                ClientProtocol::OpenAiResponses,
                &[RouteCandidate {
                    deployment_id: "deployment-a",
                    provider_material_generation: 8,
                }],
            )
            .unwrap_err();
        assert_eq!(error, ReasoningSealError::Stale);
        assert_eq!(error.code(), "reasoning_state_stale");
    }

    #[test]
    fn key_retirement_is_generation_based_not_clock_based() {
        let old_config = config(("key-1", 1), None, 1);
        let old = sealer(&old_config, &[("key-1", 1)]);
        let sealed = old
            .seal(&binding("tenant-a", "claude", "deployment-a", 7), b"state")
            .unwrap();
        let overlap = config(("key-2", 2), Some(("key-1", 1)), 2);
        assert!(
            sealer(&overlap, &[("key-2", 2), ("key-1", 1)])
                .unseal(
                    &sealed,
                    "tenant-a",
                    "claude",
                    ClientProtocol::OpenAiResponses,
                    &[RouteCandidate {
                        deployment_id: "deployment-a",
                        provider_material_generation: 7
                    }],
                )
                .is_ok()
        );
        let retired = config(("key-2", 2), None, 3);
        assert_eq!(
            sealer(&retired, &[("key-2", 2)])
                .unseal(
                    &sealed,
                    "tenant-a",
                    "claude",
                    ClientProtocol::OpenAiResponses,
                    &[RouteCandidate {
                        deployment_id: "deployment-a",
                        provider_material_generation: 7
                    }],
                )
                .unwrap_err(),
            ReasoningSealError::KeyUnavailable
        );
    }

    #[test]
    fn encoded_limits_reject_before_decode() {
        let mut config = config(("key-1", 1), None, 1);
        config.limits.max_encoded_item_bytes = 16;
        config.limits.max_cumulative_encoded_bytes = 32;
        config.limits.max_items_per_request = 2;
        let sealer = sealer(&config, &[("key-1", 1)]);
        assert_eq!(
            sealer
                .validate_request_items(["12345678901234567"])
                .unwrap_err(),
            ReasoningSealError::TooLarge
        );
        assert_eq!(
            sealer.validate_request_items(["a", "b", "c"]).unwrap_err(),
            ReasoningSealError::TooManyItems
        );
    }

    #[test]
    fn decoded_limits_are_enforced_per_item_and_cumulatively() {
        let mut config = config(("key-1", 1), None, 1);
        config.limits.max_decoded_provider_state_bytes = 8;
        config.limits.max_cumulative_decoded_bytes = 12;
        config.limits.max_items_per_request = 2;
        let reader = sealer(&config, &[("key-1", 1)]);
        let binding = binding("tenant-a", "claude", "deployment-a", 7);
        assert_eq!(
            reader.seal(&binding, b"123456789").unwrap_err(),
            ReasoningSealError::TooLarge
        );
        let mut wider_config = config.clone();
        wider_config.limits.max_decoded_provider_state_bytes = 16;
        wider_config.limits.max_cumulative_decoded_bytes = 16;
        let wider_sealer = sealer(&wider_config, &[("key-1", 1)]);
        let oversized_for_reader = wider_sealer.seal(&binding, b"123456789").unwrap();
        let candidates = [RouteCandidate {
            deployment_id: "deployment-a",
            provider_material_generation: 7,
        }];
        assert_eq!(
            reader
                .unseal(
                    &oversized_for_reader,
                    "tenant-a",
                    "claude",
                    ClientProtocol::OpenAiResponses,
                    &candidates,
                )
                .unwrap_err(),
            ReasoningSealError::TooLarge
        );
        let first = reader.seal(&binding, b"12345678").unwrap();
        let second = reader.seal(&binding, b"abcdefgh").unwrap();
        assert_eq!(
            reader
                .unseal_request_items(
                    [first.as_str(), second.as_str()],
                    "tenant-a",
                    "claude",
                    ClientProtocol::OpenAiResponses,
                    &candidates,
                )
                .unwrap_err(),
            ReasoningSealError::TooLarge
        );
    }

    #[test]
    fn decoded_cumulative_limit_must_cover_one_item() {
        let mut config = config(("key-1", 1), None, 1);
        config.limits.max_decoded_provider_state_bytes = 9;
        config.limits.max_cumulative_decoded_bytes = 8;
        let resolver = MapSecretResolver(BTreeMap::from([(
            "credential://key-1".to_string(),
            URL_SAFE_NO_PAD.encode([1u8; 32]),
        )]));
        assert!(matches!(
            ReasoningSealer::resolve(&config, &resolver),
            Err(ReasoningSealError::KeyConfigInvalid)
        ));
    }
}
