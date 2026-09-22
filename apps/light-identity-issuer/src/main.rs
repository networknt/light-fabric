use std::sync::Arc;
use std::time::Duration;

use light_identity_issuer::{
    CaMaterial, CombinedAuthorizer, FileSpentTokens, InMemoryPairingCodes, InMemoryRevocationList,
    OnDiskCaSigner, PairingGrantAuthorizer, PortalTokenAuthorizer, WorkloadIssuer,
};
use light_identity_issuer_service::{
    admin,
    config::{IssuerConfig, StatePlan, Transport},
    http, jwks, tls,
};
use tracing::{info, warn};

/// How long requests in flight get to finish after SIGTERM before the rest are cut off.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let config_path =
        std::env::var("ISSUER_CONFIG_PATH").unwrap_or_else(|_| "config/issuer.yml".to_string());
    let config =
        IssuerConfig::load(std::path::Path::new(&config_path)).map_err(std::io::Error::other)?;

    // Operator commands on the spent-token record run instead of the service.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = admin::parse(&arguments).map_err(std::io::Error::other)?;
    let state_plan = config.state().map_err(std::io::Error::other)?;
    if let Some(command) = command {
        return match state_plan {
            StatePlan::Durable { journal } => {
                println!("{}", admin::run(command, &journal).map_err(std::io::Error::other)?);
                Ok(())
            }
            StatePlan::Ephemeral => Err(std::io::Error::other(
                "there is no durable spent-token record to operate on (allow_ephemeral_state is set)",
            )
            .into()),
        };
    }

    // Open the spent-token journal before anything else: it takes the journal's
    // lock, and a corrupt or locked journal must stop the service, not be ignored.
    let spent_store = match &state_plan {
        StatePlan::Durable { journal } => {
            let store = FileSpentTokens::open(journal).map_err(std::io::Error::other)?;
            info!(journal = %journal.display(), spent = store.spent().len(), "spent-token journal opened");
            Some(Arc::new(store))
        }
        StatePlan::Ephemeral => {
            warn!(
                "spent bootstrap tokens are held in memory only: a restart forgets them, so a used token can be used again"
            );
            None
        }
    };

    // Fail before doing anything else if the listener is misconfigured: the
    // bootstrap call carries a long-lived token and must not be served in the
    // clear by accident.
    let transport = config.transport().map_err(std::io::Error::other)?;
    let tls_config = match &transport {
        Transport::Tls { certificate, key } => {
            Some(tls::server_config(certificate, key).map_err(std::io::Error::other)?)
        }
        Transport::InsecureHttp => None,
    };

    let ca_certificate_pem = std::fs::read_to_string(&config.ca_certificate_path)?;
    let ca_key_pem = std::fs::read_to_string(&config.ca_key_path)?;
    let ca_material =
        CaMaterial::from_pem(&ca_certificate_pem, &ca_key_pem).map_err(std::io::Error::other)?;

    let jwks_ca_cert_pem = config
        .jwks_ca_cert_path
        .as_ref()
        .map(std::fs::read_to_string)
        .transpose()?;

    info!(jwks_url = %config.jwks_url, "fetching initial JWKS");
    let jwks = jwks::JwksCache::spawn(
        config.jwks_url.clone(),
        Duration::from_secs(300),
        jwks_ca_cert_pem.as_deref(),
    )
    .await
    .map_err(std::io::Error::other)?;

    if config.bootstrap_roles.is_empty() {
        warn!("no bootstrap_roles configured: every Portal-token first issuance will be rejected");
    }
    if let Some(empty) = config
        .bootstrap_roles
        .iter()
        .find(|binding| binding.service_id_prefix.is_empty() || binding.role.is_empty())
    {
        return Err(std::io::Error::other(format!(
            "bootstrap_roles entry has an empty service_id_prefix or role: {empty:?}"
        ))
        .into());
    }
    let mut portal_token = PortalTokenAuthorizer::new(
        config.portal_token_issuer.clone(),
        config.portal_token_audience.clone(),
        jwks,
    );
    if let Some(store) = spent_store {
        portal_token = portal_token.with_spent_store(store);
    }
    for binding in &config.bootstrap_roles {
        portal_token =
            portal_token.with_role_binding(binding.service_id_prefix.clone(), binding.role.clone());
    }
    let pairing_codes = Arc::new(InMemoryPairingCodes::new());
    let pairing_grant = PairingGrantAuthorizer::new(Arc::clone(&pairing_codes));
    let authorizer = CombinedAuthorizer::new(portal_token, pairing_grant);

    let issuer = WorkloadIssuer::new(
        OnDiskCaSigner::new(ca_material),
        config
            .issuer_policy()
            .map_err(|error| format!("invalid issuer policy: {error}"))?,
        InMemoryRevocationList::new(),
        authorizer,
    );

    let bind_address = config.bind_address.clone();
    if config.enable_pairing_stub {
        warn!(
            "pairing-code stub enabled: POST /v1/pairing-codes has NO authentication; development stacks only"
        );
    }
    let state = Arc::new(http::AppState {
        issuer,
        pairing_codes,
        pairing_stub_enabled: config.enable_pairing_stub,
    });
    let router = http::router(state);
    match tls_config {
        Some(tls_config) => {
            let listener = std::net::TcpListener::bind(&bind_address)?;
            info!(%bind_address, "light-identity-issuer listening (TLS)");
            tls::serve_tls_until(
                listener,
                router,
                tls_config,
                tls::shutdown_signal(),
                SHUTDOWN_GRACE,
            )
            .await?;
            info!("light-identity-issuer stopped");
        }
        None => {
            warn!(%bind_address, "serving PLAIN HTTP (allow_insecure_http): the app token and CSRs cross the network in cleartext");
            let listener = tokio::net::TcpListener::bind(&bind_address).await?;
            info!(%bind_address, "light-identity-issuer listening (plain HTTP)");
            // Serving plain HTTP: stop on the signal, and do not wait on idle connections.
            let stopped = Arc::new(tokio::sync::Notify::new());
            let signalled = Arc::clone(&stopped);
            let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
                tls::shutdown_signal().await;
                signalled.notify_one();
            });
            tokio::select! {
                result = serve => result?,
                _ = async { stopped.notified().await; tokio::time::sleep(SHUTDOWN_GRACE).await } => {}
            }
            info!("light-identity-issuer stopped");
        }
    }
    Ok(())
}
