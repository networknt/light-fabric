//! Fixed A2 service API. Mount ONLY on the dedicated verified-mTLS listener.
//! Runtime integration must supply admitted permits and live per-boot owners;
//! no public endpoint can create either authority.
use crate::run_authority::RunAuthority;
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use light_axum::mtls::Peer;
use light_security::{
    SecurityRuntime,
    dual_identity::{self, Origin, RoutePolicy},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
use workflow_action::{
    Binding, Completion, Decision, DispatchState, Owner,
    ledger::{Error, Ledger},
};
#[derive(Clone)]
pub struct ActionApi {
    pub ledger: Ledger,
    pub broker: Arc<dyn RunAuthority>,
    pub security: Arc<SecurityRuntime>,
    pub policy: RoutePolicy,
    /// Administrator-approved certificate to service/replica mapping. Boot
    /// registrations and fencing generations are held in the durable store.
    pub owners: BTreeMap<String, workflow_action::GatewayRegistration>,
    pub receivers: BTreeMap<String, Vec<Uuid>>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizeRequest {
    pub reference: workflow_action::ActionReference,
    pub owner: Owner,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginResponse {
    pub decision: Decision,
    pub new_permission: bool,
}
fn status(error: Error) -> StatusCode {
    match error {
        Error::Denied => StatusCode::FORBIDDEN,
        Error::Conflict | Error::Uncertain => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}
impl ActionApi {
    async fn owner(&self, peer: &Peer, owner: &Owner, sid: &str) -> Result<(), StatusCode> {
        if owner.gateway_service != sid
            || !self
                .owners
                .get(&peer.fingerprint)
                .is_some_and(|r| r.gateway_service == sid && r.replica == owner.replica)
            || !self
                .ledger
                .is_current_owner(&peer.fingerprint, owner)
                .await
                .map_err(status)?
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
    async fn identity(
        &self,
        peer: &Peer,
        h: &HeaderMap,
        b: &Binding,
        o: &Owner,
    ) -> Result<(), StatusCode> {
        let identities =
            dual_identity::authenticate(&self.security, &self.policy, h, Some(&peer.fingerprint))
                .await
                .map_err(|e| StatusCode::from_u16(e.status).unwrap_or(StatusCode::FORBIDDEN))?;
        if identities.origin != Origin::Gateway
            || identities
                .user
                .user_id
                .as_deref()
                .and_then(|u| u.parse().ok())
                != Some(b.user_id)
            || self.policy.host_id != b.host_id
        {
            return Err(StatusCode::FORBIDDEN);
        }
        self.owner(peer, o, &identities.service_id).await?;
        let claims = workflow_invocation_contract::stable_subject_claims(&identities.user.claims);
        let digest = workflow_invocation_contract::canonical_sha256(&claims)
            .map_err(|_| StatusCode::FORBIDDEN)?;
        if digest != b.claims_digest {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
}
pub fn router(state: ActionApi) -> Result<Router, String> {
    state
        .policy
        .validate()
        .map_err(|_| "invalid action route policy")?;
    if state.owners.is_empty()
        || state.owners.iter().any(|(peer, o)| {
            o.replica.is_nil()
                || !state
                    .policy
                    .apps
                    .get(&o.gateway_service)
                    .is_some_and(|p| p.origin == Origin::Gateway && p.peer_sha256.contains(peer))
        })
    {
        return Err("invalid per-boot Gateway owners".into());
    }
    if state.receivers.iter().any(|(service, tools)| {
        tools.is_empty()
            || tools.iter().any(Uuid::is_nil)
            || !state
                .policy
                .apps
                .get(service)
                .is_some_and(|profile| profile.origin == Origin::Receiver)
    }) {
        return Err("invalid action receiver registration".into());
    }
    Ok(Router::new()
        .route(
            "/internal/workflow/actions/register-owner",
            post(register_owner),
        )
        .route("/internal/workflow/actions/inspect", post(inspect))
        .route("/internal/workflow/actions/authorize", post(authorize))
        .route("/internal/workflow/actions/begin-dispatch", post(begin))
        .route("/internal/workflow/actions/complete", post(complete))
        .route("/internal/workflow/actions/status", post(read_status))
        .route(
            "/internal/workflow/actions/receiver-authorize",
            post(receiver_authorize),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32768))
        .with_state(state))
}
async fn authorize(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(r): Json<AuthorizeRequest>,
) -> Result<Json<Decision>, StatusCode> {
    // Authenticate the service before resolving even metadata from an action.
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Gateway || s.policy.host_id != r.reference.host_id {
        return Err(StatusCode::FORBIDDEN);
    }
    s.owner(&peer, &r.owner, &sid).await?;
    let result = async {
        let binding = s.ledger.resolve(&r.reference).await.map_err(status)?;
        s.identity(&peer, &h, &binding, &r.owner).await?;
        let _guard = s
            .broker
            .lock_run_authority(
                binding.run_id,
                binding.grant_id,
                binding.host_id,
                binding.user_id,
            )
            .await
            .map_err(|_| StatusCode::FORBIDDEN)?;
        s.ledger
            .authorize(&binding, &r.owner)
            .await
            .map(Json)
            .map_err(status)
    }
    .await;
    if result.is_err() {
        s.ledger
            .record_denial(&r.reference, "authorize")
            .await
            .map_err(status)?;
    }
    result
}
async fn begin(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(d): Json<Decision>,
) -> Result<Json<BeginResponse>, StatusCode> {
    s.identity(&peer, &h, &d.binding, &d.owner).await?;
    let b = &d.binding;
    let _guard = s
        .broker
        .lock_run_authority(b.run_id, b.grant_id, b.host_id, b.user_id)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let outcome = s.ledger.begin(&d).await;
    if outcome.is_err() {
        s.ledger
            .record_denial(
                &workflow_action::ActionReference::from_binding(&d.binding),
                "begin",
            )
            .await
            .map_err(status)?;
    }
    let new_permission = outcome.map_err(status)?;
    Ok(Json(BeginResponse {
        decision: d,
        new_permission,
    }))
}
async fn complete(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(request): Json<CompletionRequest>,
) -> Result<StatusCode, StatusCode> {
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    match request {
        CompletionRequest::Owner(c) => {
            if origin != Origin::Gateway || s.policy.host_id != c.decision.binding.host_id {
                return Err(StatusCode::FORBIDDEN);
            }
            s.owner(&peer, &c.decision.owner, &sid).await?;
            s.ledger.complete(&c).await.map_err(status)?;
        }
        CompletionRequest::Reconciled(receipt) => {
            if origin != Origin::Receiver || receipt.host_id != s.policy.host_id {
                return Err(StatusCode::FORBIDDEN);
            }
            let binding = s
                .ledger
                .reconciliation_binding(receipt.host_id, receipt.action_id, receipt.generation)
                .await
                .map_err(status)?;
            if !s
                .receivers
                .get(&sid)
                .is_some_and(|tools| tools.contains(&binding.tool_ref))
            {
                return Err(StatusCode::FORBIDDEN);
            }
            s.ledger.reconcile(&receipt).await.map_err(status)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompletionRequest {
    Owner(Completion),
    Reconciled(workflow_action::Reconciliation),
}
async fn read_status(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(r): Json<workflow_action::StatusRequest>,
) -> Result<Json<DispatchState>, StatusCode> {
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Gateway || r.reference.host_id != s.policy.host_id {
        return Err(StatusCode::FORBIDDEN);
    }
    s.owner(&peer, &r.owner, &sid).await?;
    let b = s.ledger.resolve(&r.reference).await.map_err(status)?;
    s.identity(&peer, &h, &b, &r.owner).await?;
    let _guard = s
        .broker
        .lock_run_authority(b.run_id, b.grant_id, b.host_id, b.user_id)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    s.ledger
        .latest_status(&b, &r.owner)
        .await
        .map(Json)
        .map_err(status)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionSettings {
    #[serde(default)]
    pub workflow_agents: BTreeMap<String, uuid::Uuid>,
    #[serde(default)]
    pub receivers: BTreeMap<String, Vec<Uuid>>,
    pub outbound: crate::bound_mcp::Config,
    pub tls: light_axum::mtls::Config,
    pub policy: RoutePolicy,
    pub owners: BTreeMap<String, workflow_action::GatewayRegistration>,
}

async fn register_owner(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(r): Json<workflow_action::RegisterOwner>,
) -> Result<Json<Owner>, StatusCode> {
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Gateway
        || r.gateway_service != sid
        || !s
            .owners
            .get(&peer.fingerprint)
            .is_some_and(|v| v.gateway_service == sid && v.replica == r.replica)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    s.ledger
        .register_owner(&peer.fingerprint, &r)
        .await
        .map(Json)
        .map_err(status)
}

async fn inspect(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(r): Json<AuthorizeRequest>,
) -> Result<Json<Binding>, StatusCode> {
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Gateway || r.reference.host_id != s.policy.host_id {
        return Err(StatusCode::FORBIDDEN);
    }
    s.owner(&peer, &r.owner, &sid).await?;
    let b = s.ledger.resolve(&r.reference).await.map_err(status)?;
    s.identity(&peer, &h, &b, &r.owner).await?;
    let _guard = s
        .broker
        .lock_run_authority(b.run_id, b.grant_id, b.host_id, b.user_id)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    s.ledger.inspect(&b, &r.owner).await.map_err(status)?;
    Ok(Json(b))
}

async fn receiver_authorize(
    State(s): State<ActionApi>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    h: HeaderMap,
    Json(r): Json<light_client::workflow_receivers::Request>,
) -> Result<Json<Binding>, StatusCode> {
    let identity = dual_identity::authenticate(&s.security, &s.policy, &h, Some(&peer.fingerprint))
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    if identity.origin != Origin::Receiver
        || identity.action_reference != Some(r.action_id)
        || r.host_id != s.policy.host_id
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let binding = s
        .ledger
        .receiver_binding(r.host_id, r.action_id)
        .await
        .map_err(status)?;
    if !s
        .receivers
        .get(&identity.service_id)
        .is_some_and(|tools| tools.contains(&binding.tool_ref))
        || identity
            .user
            .user_id
            .as_deref()
            .and_then(|value| value.parse::<Uuid>().ok())
            != Some(binding.user_id)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let claims = workflow_invocation_contract::stable_subject_claims(&identity.user.claims);
    if workflow_invocation_contract::canonical_sha256(&claims).map_err(|_| StatusCode::FORBIDDEN)?
        != binding.claims_digest
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let _grant = s
        .broker
        .lock_run_authority(
            binding.run_id,
            binding.grant_id,
            binding.host_id,
            binding.user_id,
        )
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(Json(binding))
}
