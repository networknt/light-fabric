//! CSR-in, certificate-out HTTP surface. Bootstrap for the very first call
//! is credential-based (Portal token or pairing grant), never mTLS, per the
//! design doc: the first call from any workload cannot yet present a client
//! certificate that this issuer signed.
use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use light_identity_issuer::{
    BootstrapCredential, CombinedAuthorizer, InMemoryPairingCodes, InMemoryRevocationList,
    IssuedCertificate, IssuerError, LeafIdentity, OnDiskCaSigner, RenewalProof, WorkloadIssuer,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::jwks::JwksCache;

type Issuer = WorkloadIssuer<
    OnDiskCaSigner,
    InMemoryRevocationList,
    CombinedAuthorizer<Arc<JwksCache>, Arc<InMemoryPairingCodes>>,
>;

pub struct AppState {
    pub issuer: Issuer,
    /// Exposed so an out-of-band pairing step (Portal-initiated, per the
    /// Light CLI design) can issue codes; not reachable over this HTTP API
    /// itself, since minting a pairing grant is Portal's job, not this
    /// service's.
    pub pairing_codes: Arc<InMemoryPairingCodes>,
    /// Whether `POST /v1/pairing-codes` is served. Off by default: the route
    /// has no authentication, so anyone who can reach the port could mint a
    /// code for any service ID and role. It exists only to stub Portal's
    /// pairing UI on a development stack.
    pub pairing_stub_enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum CredentialRequest {
    PortalToken { token: String },
    PairingGrant { code: String },
}

impl From<CredentialRequest> for BootstrapCredential {
    fn from(value: CredentialRequest) -> Self {
        match value {
            CredentialRequest::PortalToken { token } => BootstrapCredential::PortalToken(token),
            CredentialRequest::PairingGrant { code } => BootstrapCredential::PairingGrant(code),
        }
    }
}

/// First issuance carries no identity: the service ID, role and install ID are
/// decided by the issuer from the credential. Unknown fields are rejected, so a
/// client that still sends an `identity` is told so instead of being ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FirstIssuanceRequest {
    env_tag: String,
    credential: CredentialRequest,
    csr_der: String,
}

/// Proof that the caller holds the private key of the certificate it presents,
/// per `light_identity_issuer::renewal_message`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenewalProofRequest {
    timestamp: i64,
    nonce: String,
    /// Base64 (standard alphabet) signature made with the OLD private key.
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenewalRequest {
    env_tag: String,
    presented_certificate_der: String,
    csr_der: String,
    proof: RenewalProofRequest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CertificateResponse {
    certificate_der: String,
    certificate_pem: String,
    /// The issuing CA certificate. A client presents `[leaf, CA]` in its TLS
    /// handshake, because the Gateway derives the issuer digest from the
    /// second certificate in the chain.
    ca_certificate_pem: String,
    /// `certificate_pem` followed by `ca_certificate_pem`, in the order to
    /// present them.
    chain_pem: String,
    install_id: Uuid,
    service_id: String,
    role: String,
    not_after: String,
    /// Policy deadline at which the workload must renew this certificate.
    renew_at: String,
}

impl From<IssuedCertificate> for CertificateResponse {
    fn from(issued: IssuedCertificate) -> Self {
        let mut chain_pem = issued.certificate_pem.trim_end().to_string();
        chain_pem.push('\n');
        chain_pem.push_str(&issued.ca_certificate_pem);
        Self {
            certificate_der: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &issued.certificate_der,
            ),
            certificate_pem: issued.certificate_pem,
            ca_certificate_pem: issued.ca_certificate_pem,
            chain_pem,
            install_id: issued.identity.install_id,
            service_id: issued.identity.service_id,
            role: issued.identity.role,
            not_after: issued
                .not_after
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            renew_at: issued
                .renew_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        }
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn status_for(error: &IssuerError) -> StatusCode {
    match error {
        IssuerError::UnknownEnvironment(_) => StatusCode::BAD_REQUEST,
        IssuerError::InvalidCsr(_) => StatusCode::BAD_REQUEST,
        IssuerError::InvalidPresentedCertificate(_) => StatusCode::BAD_REQUEST,
        IssuerError::InvalidCaMaterial(_) => StatusCode::INTERNAL_SERVER_ERROR,
        IssuerError::InvalidIdentity(_) => StatusCode::BAD_REQUEST,
        IssuerError::UnsupportedKeyAlgorithm => StatusCode::BAD_REQUEST,
        IssuerError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        IssuerError::PresentedCertificateNotIssuedByThisCa => StatusCode::UNAUTHORIZED,
        IssuerError::PresentedCertificateExpired => StatusCode::UNAUTHORIZED,
        IssuerError::PossessionProofInvalid => StatusCode::UNAUTHORIZED,
        IssuerError::PossessionProofStale => StatusCode::UNAUTHORIZED,
        IssuerError::ReplayedRenewal => StatusCode::UNAUTHORIZED,
        IssuerError::EnvironmentMismatch => StatusCode::FORBIDDEN,
        IssuerError::InvalidLifetime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        IssuerError::Revoked(_) => StatusCode::FORBIDDEN,
        IssuerError::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
        IssuerError::Signing(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn issuer_response(result: Result<IssuedCertificate, IssuerError>) -> axum::response::Response {
    match result {
        Ok(certificate) => {
            (StatusCode::OK, Json(CertificateResponse::from(certificate))).into_response()
        }
        Err(error) => {
            // Storage errors name files and disks: keep that in the server's log
            // and tell the caller only that the request can be retried.
            let message = if matches!(error, IssuerError::Storage(_)) {
                tracing::error!(%error, "issuer state storage failed; refusing the request");
                "issuer state storage is unavailable; the request was refused and can be retried"
                    .to_string()
            } else {
                error.to_string()
            };
            (status_for(&error), Json(ErrorResponse { error: message })).into_response()
        }
    }
}

fn decode_der(base64_der: &str) -> Result<Vec<u8>, String> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_der)
        .map_err(|err| format!("invalid base64 in request: {err}"))
}

fn bad_request(message: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse { error: message }),
    )
        .into_response()
}

fn blocking_task_failed(error: tokio::task::JoinError) -> axum::response::Response {
    tracing::error!(%error, "issuer blocking task failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "issuer could not complete the request".into(),
        }),
    )
        .into_response()
}

async fn issue_first(
    State(state): State<Arc<AppState>>,
    Json(request): Json<FirstIssuanceRequest>,
) -> axum::response::Response {
    let csr_der = match decode_der(&request.csr_der) {
        Ok(der) => der,
        Err(message) => return bad_request(message),
    };
    let result = tokio::task::spawn_blocking(move || {
        state
            .issuer
            .issue_first(&request.env_tag, request.credential.into(), &csr_der)
    })
    .await;
    match result {
        Ok(result) => issuer_response(result),
        Err(error) => blocking_task_failed(error),
    }
}

async fn renew(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RenewalRequest>,
) -> axum::response::Response {
    let presented = match decode_der(&request.presented_certificate_der) {
        Ok(der) => der,
        Err(message) => return bad_request(message),
    };
    let csr_der = match decode_der(&request.csr_der) {
        Ok(der) => der,
        Err(message) => return bad_request(message),
    };
    let signature = match decode_der(&request.proof.signature) {
        Ok(bytes) => bytes,
        Err(message) => return bad_request(message),
    };
    let proof = RenewalProof {
        timestamp: request.proof.timestamp,
        nonce: request.proof.nonce,
        signature,
    };
    let result = tokio::task::spawn_blocking(move || {
        state
            .issuer
            .renew(&request.env_tag, &presented, &csr_der, &proof)
    })
    .await;
    match result {
        Ok(result) => issuer_response(result),
        Err(error) => blocking_task_failed(error),
    }
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingCodeRequest {
    code: String,
    install_id: Uuid,
    service_id: String,
    role: String,
}

/// Manually issues a one-time pairing code. Stubs Portal's pairing UI,
/// per the implementation plan's Phase 1 scope ("This item can stub the
/// pairing flow with a manually issued one-time code if Phase 2 starts
/// before Portal's pairing UI exists"); an operator or the CLI's own
/// pairing step calls this until that UI exists, not end users at large.
async fn issue_pairing_code(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PairingCodeRequest>,
) -> StatusCode {
    state.pairing_codes.issue(
        request.code,
        LeafIdentity {
            env_tag: None,
            install_id: request.install_id,
            service_id: request.service_id,
            role: request.role,
        },
    );
    StatusCode::CREATED
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/v1/csr", post(issue_first))
        .route("/v1/renew", post(renew))
        .route("/health", axum::routing::get(health));
    if state.pairing_stub_enabled {
        router = router.route("/v1/pairing-codes", post(issue_pairing_code));
    }
    router.with_state(state)
}
