//! Real `FirstIssuanceAuthorizer` implementations. Phase 1 of the
//! implementation plan: verify the existing long-lived Portal token for
//! service workloads, and redeem a pairing grant for CLI installs that have
//! no pre-provisioned token.
//!
//! Deliberately does not depend on `light-security`'s `SecurityRuntime`:
//! that type requires a full `RuntimeConfig`/config-loader/module-registry
//! stack to construct, which is far more than this narrow crate needs.
//! Instead this module verifies signature and claims directly with
//! `jsonwebtoken`, reusing `oauth-workflow-contract`'s `TokenUse::validate`
//! for the same purpose-claim check the production code performs, and
//! leaves JWKS/decoding-key resolution to an injected trait so the Phase 2
//! app layer owns fetching and caching keys from `light-oauth`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use oauth_workflow_contract::TokenUse;
use uuid::Uuid;

use crate::{Authorized, BootstrapCredential, FirstIssuanceAuthorizer, IssuerError, LeafIdentity};

/// Resolves the decoding key for a token's `kid`. Implemented by the Phase 2
/// app layer against `light-oauth`'s JWKS endpoint (with whatever caching it
/// needs); this crate only consumes the resolved key.
pub trait DecodingKeyResolver: Send + Sync {
    fn resolve(&self, kid: &str) -> Option<DecodingKey>;
}

impl<T: DecodingKeyResolver + ?Sized> DecodingKeyResolver for std::sync::Arc<T> {
    fn resolve(&self, kid: &str) -> Option<DecodingKey> {
        (**self).resolve(kid)
    }
}

/// Where the identities (JWT `jti`) of spent bootstrap tokens are recorded.
///
/// `try_consume` must be atomic and, for a durable store, must not report `true`
/// until the spend is safely recorded. If it cannot record it, it must return an
/// error and NOT treat the token as spent: the issuer then refuses the request
/// (fail closed) and the token can be retried.
pub trait SpentTokenStore: Send + Sync {
    /// `Ok(true)` if `key` was not spent before and now is; `Ok(false)` if it
    /// already was.
    fn try_consume(&self, key: &str) -> Result<bool, IssuerError>;
    /// Forget that `key` was spent (explicit re-bootstrap, or undoing a spend
    /// when signing failed). A no-op if it was not spent.
    fn reset(&self, key: &str) -> Result<(), IssuerError>;
}

/// The spent-token record in memory only. An issuer restart forgets every spend,
/// so a token that was used can be used again. Use `FileSpentTokens` for anything
/// but a throwaway run.
#[derive(Default)]
pub struct InMemorySpentTokens {
    used: Mutex<HashSet<String>>,
}

impl SpentTokenStore for InMemorySpentTokens {
    fn try_consume(&self, key: &str) -> Result<bool, IssuerError> {
        Ok(self
            .used
            .lock()
            .expect("replay guard lock")
            .insert(key.to_string()))
    }

    fn reset(&self, key: &str) -> Result<(), IssuerError> {
        self.used.lock().expect("replay guard lock").remove(key);
        Ok(())
    }
}

/// Tracks which bootstrap token identities have already been spent on a first
/// issuance, so the same long-lived Portal token cannot be replayed indefinitely
/// as a standing renewal credential. Renewal never consults this: only
/// `issue_first` does.
pub struct BootstrapReplayGuard {
    store: Arc<dyn SpentTokenStore>,
}

impl BootstrapReplayGuard {
    /// A guard whose record is in memory only.
    pub fn new() -> Self {
        Self::with_store(Arc::new(InMemorySpentTokens::default()))
    }

    pub fn with_store(store: Arc<dyn SpentTokenStore>) -> Self {
        Self { store }
    }

    /// `Ok(true)` (and records the key) only the first time a given key is seen.
    fn try_consume(&self, key: &str) -> Result<bool, IssuerError> {
        self.store.try_consume(key)
    }

    /// Explicit re-bootstrap: clears a previously spent identity so it may
    /// authorize first issuance again. Also how a spend is undone when signing
    /// fails after the token was accepted.
    pub fn reset(&self, key: &str) -> Result<(), IssuerError> {
        self.store.reset(key)
    }
}

impl Default for BootstrapReplayGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Verifies the long-lived Portal token every service workload already
/// obtains today to authenticate to config-server and Controller, and
/// authorizes exactly one first issuance per token identity.
///
/// The certificate identity comes from the token, never from the request:
/// the service ID is the token's `sid`, the environment must equal the token's
/// `env`, the role comes from a server-side binding for that service ID, and
/// the install ID is generated here. A token with no binding for its `sid` is
/// rejected, so with no bindings configured every token is rejected.
pub struct PortalTokenAuthorizer<K> {
    issuer: String,
    audience: String,
    keys: K,
    replay_guard: BootstrapReplayGuard,
    role_bindings: Vec<(String, String)>,
}

impl<K: DecodingKeyResolver> PortalTokenAuthorizer<K> {
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>, keys: K) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            keys,
            replay_guard: BootstrapReplayGuard::new(),
            role_bindings: Vec::new(),
        }
    }

    /// Record spent tokens in `store` instead of in memory. Use a durable store
    /// so the once-only guarantee survives an issuer restart.
    pub fn with_spent_store(mut self, store: Arc<dyn SpentTokenStore>) -> Self {
        self.replay_guard = BootstrapReplayGuard::with_store(store);
        self
    }

    /// Allow tokens whose `sid` starts with `service_id_prefix` to enroll, and
    /// bind them to `role`. Service IDs carry their version
    /// (`com.networknt.light-cli-1.0.0`), so a prefix such as
    /// `com.networknt.light-cli-` covers every release; the longest matching
    /// prefix wins. An empty prefix is ignored, since it would match every
    /// token.
    pub fn with_role_binding(
        mut self,
        service_id_prefix: impl Into<String>,
        role: impl Into<String>,
    ) -> Self {
        let prefix = service_id_prefix.into();
        if !prefix.is_empty() {
            self.role_bindings.push((prefix, role.into()));
        }
        self
    }

    fn role_for(&self, service_id: &str) -> Option<&str> {
        self.role_bindings
            .iter()
            .filter(|(prefix, _)| service_id.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, role)| role.as_str())
    }

    /// The replay identity for a token: its `jti` claim if present, else a
    /// `sub`+`iat` fallback for legacy tokens minted without one.
    fn replay_key(claims: &serde_json::Value) -> Option<String> {
        if let Some(jti) = claims.get("jti").and_then(serde_json::Value::as_str) {
            return Some(jti.to_string());
        }
        let sub = claims.get("sub").and_then(serde_json::Value::as_str)?;
        let iat = claims.get("iat").and_then(serde_json::Value::as_i64)?;
        Some(format!("{sub}:{iat}"))
    }
}

impl<K: DecodingKeyResolver> FirstIssuanceAuthorizer for PortalTokenAuthorizer<K> {
    fn authorize(
        &self,
        credential: &BootstrapCredential,
        env_tag: &str,
    ) -> Result<Authorized, IssuerError> {
        let BootstrapCredential::PortalToken(token) = credential else {
            return Err(IssuerError::Unauthorized(
                "credential kind not accepted here",
            ));
        };

        let invalid = || IssuerError::Unauthorized("invalid token");
        let header = decode_header(token).map_err(|_| invalid())?;
        let kid = header.kid.ok_or_else(invalid)?;
        let key = self.keys.resolve(&kid).ok_or_else(invalid)?;

        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.validate_nbf = true;

        let decoded =
            decode::<serde_json::Value>(token, &key, &validation).map_err(|_| invalid())?;
        let claims = &decoded.claims;
        TokenUse::App.validate(claims).map_err(|_| invalid())?;

        // Everything below is checked before the token is spent, so a request
        // that is refused for a fixable reason does not burn it.
        let service_id = claims
            .get("sid")
            .and_then(serde_json::Value::as_str)
            .ok_or(IssuerError::Unauthorized("token has no service id"))?;
        let token_env = claims
            .get("env")
            .and_then(serde_json::Value::as_str)
            .ok_or(IssuerError::Unauthorized("token has no environment"))?;
        if token_env != env_tag {
            return Err(IssuerError::Unauthorized(
                "token environment does not match the request",
            ));
        }
        let role = self.role_for(service_id).ok_or(IssuerError::Unauthorized(
            "token service is not enrolled for a role",
        ))?;

        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: service_id.to_string(),
            role: role.to_string(),
        };
        identity.validate()?;

        let replay_key = Self::replay_key(claims).ok_or_else(invalid)?;
        // A storage failure is an error, not "already used": the token was not
        // spent, and the caller may retry once storage recovers.
        if !self.replay_guard.try_consume(&replay_key)? {
            return Err(IssuerError::Unauthorized("token already used"));
        }
        Ok(Authorized {
            identity,
            spent_key: Some(replay_key),
        })
    }

    fn refund(&self, credential: &BootstrapCredential, authorized: &Authorized) {
        if let (BootstrapCredential::PortalToken(_), Some(key)) =
            (credential, authorized.spent_key.as_deref())
        {
            // If undoing fails the token simply stays spent, the safe direction.
            let _ = self.replay_guard.reset(key);
        }
    }
}

/// Redeems a single-use pairing grant for a CLI install with no
/// pre-provisioned Portal token, per the Light CLI design's pairing flow.
/// Portal mints the code out-of-band (today: an interactive step in the
/// portal-view UI); this trait is the redemption seam that flow calls into.
pub trait PairingCodeStore: Send + Sync {
    /// Redeem `code`, consuming it so it cannot be redeemed twice, and return
    /// the identity it was minted for. `None` if the code is unknown or used.
    fn redeem(&self, code: &str) -> Option<LeafIdentity>;
}

impl<T: PairingCodeStore + ?Sized> PairingCodeStore for std::sync::Arc<T> {
    fn redeem(&self, code: &str) -> Option<LeafIdentity> {
        (**self).redeem(code)
    }
}

pub struct PairingGrantAuthorizer<P> {
    store: P,
}

impl<P: PairingCodeStore> PairingGrantAuthorizer<P> {
    pub fn new(store: P) -> Self {
        Self { store }
    }
}

impl<P: PairingCodeStore> FirstIssuanceAuthorizer for PairingGrantAuthorizer<P> {
    fn authorize(
        &self,
        credential: &BootstrapCredential,
        _env_tag: &str,
    ) -> Result<Authorized, IssuerError> {
        let BootstrapCredential::PairingGrant(code) = credential else {
            return Err(IssuerError::Unauthorized(
                "credential kind not accepted here",
            ));
        };
        let identity = self.store.redeem(code).ok_or(IssuerError::Unauthorized(
            "invalid or already used pairing code",
        ))?;
        identity.validate()?;
        // A pairing code is not refunded if signing then fails: it is cheap
        // to mint another, and re-arming one would need a store operation the
        // seam does not have.
        Ok(Authorized {
            identity,
            spent_key: None,
        })
    }
}

/// In-memory `PairingCodeStore` for tests and for a first working CLI
/// pairing flow before any persistent store is needed. A restart forgets every
/// unredeemed code.
pub struct InMemoryPairingCodes {
    codes: Mutex<std::collections::HashMap<String, LeafIdentity>>,
}

impl InMemoryPairingCodes {
    pub fn new() -> Self {
        Self {
            codes: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Issue a one-time code for exactly `identity`: the service ID, role and
    /// install ID are fixed here, at mint time, and the redeemer cannot
    /// change them. No expiry tracking; `redeem` is single-use regardless.
    pub fn issue(&self, code: impl Into<String>, identity: LeafIdentity) {
        self.codes
            .lock()
            .expect("pairing code lock")
            .insert(code.into(), identity);
    }
}

impl Default for InMemoryPairingCodes {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingCodeStore for InMemoryPairingCodes {
    fn redeem(&self, code: &str) -> Option<LeafIdentity> {
        self.codes.lock().expect("pairing code lock").remove(code)
    }
}

/// Dispatches to the Portal-token authorizer for service workloads or the
/// pairing-grant authorizer for CLI installs, based on which
/// `BootstrapCredential` variant is presented. The one `FirstIssuanceAuthorizer`
/// a `WorkloadIssuer` needs, composed from the two real implementations.
pub struct CombinedAuthorizer<K, P> {
    portal_token: PortalTokenAuthorizer<K>,
    pairing_grant: PairingGrantAuthorizer<P>,
}

impl<K: DecodingKeyResolver, P: PairingCodeStore> CombinedAuthorizer<K, P> {
    pub fn new(
        portal_token: PortalTokenAuthorizer<K>,
        pairing_grant: PairingGrantAuthorizer<P>,
    ) -> Self {
        Self {
            portal_token,
            pairing_grant,
        }
    }
}

impl<K: DecodingKeyResolver, P: PairingCodeStore> FirstIssuanceAuthorizer
    for CombinedAuthorizer<K, P>
{
    fn authorize(
        &self,
        credential: &BootstrapCredential,
        env_tag: &str,
    ) -> Result<Authorized, IssuerError> {
        match credential {
            BootstrapCredential::PortalToken(_) => self.portal_token.authorize(credential, env_tag),
            BootstrapCredential::PairingGrant(_) => {
                self.pairing_grant.authorize(credential, env_tag)
            }
        }
    }

    fn refund(&self, credential: &BootstrapCredential, authorized: &Authorized) {
        match credential {
            BootstrapCredential::PortalToken(_) => self.portal_token.refund(credential, authorized),
            BootstrapCredential::PairingGrant(_) => {
                self.pairing_grant.refund(credential, authorized)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    struct FixedKey(DecodingKey);

    impl DecodingKeyResolver for FixedKey {
        fn resolve(&self, kid: &str) -> Option<DecodingKey> {
            (kid == "test-kid").then(|| self.0.clone())
        }
    }

    fn signed_token(claims: &serde_json::Value, secret: &[u8]) -> String {
        let header = Header {
            kid: Some("test-kid".to_string()),
            ..Header::default()
        };
        encode(&header, claims, &EncodingKey::from_secret(secret)).expect("sign token")
    }

    const SECRET: &[u8] = b"test-secret";

    fn authorizer() -> PortalTokenAuthorizer<FixedKey> {
        PortalTokenAuthorizer::new(
            "light-oauth",
            "light-fabric",
            FixedKey(DecodingKey::from_secret(SECRET)),
        )
        .with_role_binding("com.networknt.light-cli-", "cli")
        .with_role_binding("com.networknt.agent.", "agent")
    }

    fn claims(jti: &str) -> serde_json::Value {
        json!({
            "iss": "light-oauth",
            "aud": "light-fabric",
            "sub": "client-1",
            "sid": "com.networknt.light-cli-1.0.0",
            "env": "dev",
            "iat": 1_700_000_000,
            "exp": 4_102_444_800i64,
            "token_use": "app",
            "jti": jti,
        })
    }

    fn credential(claims: &serde_json::Value) -> BootstrapCredential {
        BootstrapCredential::PortalToken(signed_token(claims, SECRET))
    }

    #[test]
    fn identity_comes_from_the_token_not_the_caller() {
        let authorized = authorizer()
            .authorize(&credential(&claims("token-1")), "dev")
            .expect("valid token authorizes");

        assert_eq!(
            authorized.identity.service_id,
            "com.networknt.light-cli-1.0.0"
        );
        assert_eq!(authorized.identity.role, "cli");
        assert_ne!(authorized.identity.install_id, Uuid::nil());
        assert_eq!(authorized.spent_key.as_deref(), Some("token-1"));
    }

    #[test]
    fn each_first_issuance_gets_its_own_install_id() {
        let a = authorizer();
        let first = a.authorize(&credential(&claims("token-a")), "dev").unwrap();
        let second = a.authorize(&credential(&claims("token-b")), "dev").unwrap();
        assert_ne!(first.identity.install_id, second.identity.install_id);
    }

    #[test]
    fn the_longest_matching_role_binding_wins() {
        let a = PortalTokenAuthorizer::new(
            "light-oauth",
            "light-fabric",
            FixedKey(DecodingKey::from_secret(SECRET)),
        )
        .with_role_binding("com.networknt.", "generic")
        .with_role_binding("com.networknt.agent.", "agent");

        let mut agent = claims("token-agent");
        agent["sid"] = json!("com.networknt.agent.codex-personal-1.0.0");
        let authorized = a.authorize(&credential(&agent), "dev").unwrap();
        assert_eq!(authorized.identity.role, "agent");
    }

    #[test]
    fn a_token_for_a_service_with_no_role_binding_is_rejected() {
        let mut other = claims("token-other");
        other["sid"] = json!("com.networknt.light-gateway-1.0.0");
        assert!(matches!(
            authorizer().authorize(&credential(&other), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn with_no_bindings_configured_every_token_is_rejected() {
        let unbound = PortalTokenAuthorizer::new(
            "light-oauth",
            "light-fabric",
            FixedKey(DecodingKey::from_secret(SECRET)),
        );
        assert!(matches!(
            unbound.authorize(&credential(&claims("token-x")), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn an_empty_role_binding_prefix_is_ignored() {
        let unbound = PortalTokenAuthorizer::new(
            "light-oauth",
            "light-fabric",
            FixedKey(DecodingKey::from_secret(SECRET)),
        )
        .with_role_binding("", "cli");
        assert!(matches!(
            unbound.authorize(&credential(&claims("token-y")), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn a_token_for_another_environment_is_rejected_and_not_spent() {
        let a = authorizer();
        let token = credential(&claims("token-env"));
        assert!(matches!(
            a.authorize(&token, "prod"),
            Err(IssuerError::Unauthorized(_))
        ));
        // Refused for a fixable reason, so the token was not burned.
        assert!(a.authorize(&token, "dev").is_ok());
    }

    #[test]
    fn a_token_without_a_service_id_or_environment_is_rejected() {
        for missing in ["sid", "env"] {
            let mut c = claims("token-missing");
            c.as_object_mut().unwrap().remove(missing);
            assert!(matches!(
                authorizer().authorize(&credential(&c), "dev"),
                Err(IssuerError::Unauthorized(_))
            ));
        }
    }

    #[test]
    fn wrong_signature_is_rejected() {
        let token =
            BootstrapCredential::PortalToken(signed_token(&claims("token-2"), b"wrong-secret"));
        assert!(matches!(
            authorizer().authorize(&token, "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn wrong_token_use_is_rejected() {
        let mut c = claims("token-3");
        c["token_use"] = json!("user");
        assert!(matches!(
            authorizer().authorize(&credential(&c), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn replayed_token_identity_is_rejected_on_second_first_issuance() {
        let a = authorizer();
        let token = credential(&claims("token-4"));
        assert!(a.authorize(&token, "dev").is_ok());
        assert!(matches!(
            a.authorize(&token, "dev"),
            Err(IssuerError::Unauthorized("token already used"))
        ));
    }

    #[test]
    fn explicit_reset_allows_re_bootstrap() {
        let a = authorizer();
        let token = credential(&claims("token-5"));
        assert!(a.authorize(&token, "dev").is_ok());
        a.replay_guard.reset("token-5").unwrap();
        assert!(a.authorize(&token, "dev").is_ok());
    }

    #[test]
    fn refund_undoes_a_spend() {
        let a = authorizer();
        let token = credential(&claims("token-6"));
        let authorized = a.authorize(&token, "dev").unwrap();
        assert!(a.authorize(&token, "dev").is_err());

        a.refund(&token, &authorized);
        assert!(a.authorize(&token, "dev").is_ok());
    }

    fn durable(path: &std::path::Path) -> PortalTokenAuthorizer<FixedKey> {
        authorizer().with_spent_store(Arc::new(crate::FileSpentTokens::open(path).unwrap()))
    }

    #[test]
    fn a_spent_token_stays_spent_across_an_issuer_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("spent-tokens.jsonl");
        let token = credential(&claims("token-durable"));

        {
            let first_run = durable(&journal);
            assert!(first_run.authorize(&token, "dev").is_ok());
        } // the process exits

        let second_run = durable(&journal);
        assert!(
            matches!(
                second_run.authorize(&token, "dev"),
                Err(IssuerError::Unauthorized("token already used"))
            ),
            "a restart must not hand a spent token back"
        );
    }

    #[test]
    fn a_refund_is_durable_too() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("spent-tokens.jsonl");
        let token = credential(&claims("token-refunded"));
        {
            let first_run = durable(&journal);
            let authorized = first_run.authorize(&token, "dev").unwrap();
            first_run.refund(&token, &authorized); // signing failed
        }
        assert!(
            durable(&journal).authorize(&token, "dev").is_ok(),
            "the undo survived the restart"
        );
    }

    struct BrokenStore;

    impl SpentTokenStore for BrokenStore {
        fn try_consume(&self, _key: &str) -> Result<bool, IssuerError> {
            Err(IssuerError::Storage("disk full".into()))
        }
        fn reset(&self, _key: &str) -> Result<(), IssuerError> {
            Err(IssuerError::Storage("disk full".into()))
        }
    }

    #[test]
    fn if_the_record_cannot_be_written_issuance_is_refused_not_waved_through() {
        let a = authorizer().with_spent_store(Arc::new(BrokenStore));
        let result = a.authorize(&credential(&claims("token-nodisk")), "dev");
        assert!(
            matches!(result, Err(IssuerError::Storage(_))),
            "fail closed: {result:?}"
        );
    }

    #[test]
    fn a_pairing_grant_redeems_once_for_the_identity_it_was_minted_for() {
        let store = InMemoryPairingCodes::new();
        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.light-cli-1.0.0".into(),
            role: "cli".into(),
        };
        store.issue("pair-code", identity.clone());

        let a = PairingGrantAuthorizer::new(store);
        let credential = BootstrapCredential::PairingGrant("pair-code".to_string());

        let authorized = a.authorize(&credential, "dev").expect("redeems");
        assert_eq!(authorized.identity, identity);
        assert!(matches!(
            a.authorize(&credential, "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn an_unknown_pairing_code_is_rejected() {
        let a = PairingGrantAuthorizer::new(InMemoryPairingCodes::new());
        assert!(matches!(
            a.authorize(&BootstrapCredential::PairingGrant("nope".into()), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }

    #[test]
    fn a_credential_of_the_wrong_kind_is_rejected() {
        let a = authorizer();
        assert!(matches!(
            a.authorize(&BootstrapCredential::PairingGrant("x".into()), "dev"),
            Err(IssuerError::Unauthorized(_))
        ));
    }
}
