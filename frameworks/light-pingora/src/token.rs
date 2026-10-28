use crate::policy_adapter::PolicyRequest;
use crate::security::HandlerRejection;
pub use gateway_policy::token::{
    CLIENT_FILE, CLIENT_TOKEN_CONFIG_NAME, CLIENT_TOKEN_MODULE_ID, ClientOauthConfig,
    ClientRequestConfig, ClientTlsConfig, ClientTokenConfig, OAuthAuthorizationCodeConfig,
    OAuthClientCredentialsConfig, OAuthRefreshTokenConfig, OAuthTokenCacheConfig, OAuthTokenConfig,
    OAuthTokenExchangeConfig, SCOPE_TOKEN_HEADER, SIDECAR_CONFIG_NAME, SIDECAR_FILE,
    SIDECAR_LEGACY_FILE, SIDECAR_MODULE_ID, SidecarTrafficConfig, TOKEN_CACHE_NAME,
    TOKEN_CONFIG_NAME, TOKEN_FILE, TOKEN_LEGACY_FILE, TOKEN_MODULE_ID, TokenHandlerConfig,
    TokenRuntime,
};
use light_runtime::{ModuleKind, RuntimeCache, RuntimeConfig, RuntimeError, client_config_masks};
use pingora::prelude::Session;
use std::sync::Arc;

pub fn load_token_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<TokenRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let handler = match runtime_config
        .module_registry
        .load_config::<TokenHandlerConfig>(runtime_config, TOKEN_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == TOKEN_FILE => match runtime_config
            .module_registry
            .load_config::<TokenHandlerConfig>(runtime_config, TOKEN_LEGACY_FILE)
        {
            Ok(config) => config,
            Err(RuntimeError::MissingConfig(file)) if file == TOKEN_LEGACY_FILE => {
                TokenHandlerConfig::default()
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        TOKEN_MODULE_ID,
        TOKEN_CONFIG_NAME,
        ModuleKind::Framework,
        &handler,
        [],
        handler.enabled,
        Some(handler.enabled),
        true,
    )?;

    if !handler.enabled {
        return Ok(None);
    }

    let sidecar = match runtime_config
        .module_registry
        .load_config::<SidecarTrafficConfig>(runtime_config, SIDECAR_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == SIDECAR_FILE => match runtime_config
            .module_registry
            .load_config::<SidecarTrafficConfig>(runtime_config, SIDECAR_LEGACY_FILE)
        {
            Ok(config) => config,
            Err(RuntimeError::MissingConfig(file)) if file == SIDECAR_LEGACY_FILE => {
                SidecarTrafficConfig::default()
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };
    runtime_config.module_registry.register_loaded_config(
        SIDECAR_MODULE_ID,
        SIDECAR_CONFIG_NAME,
        ModuleKind::Framework,
        &sidecar,
        [],
        true,
        Some(true),
        true,
    )?;

    let client = load_client_config(runtime_config)?;

    let runtime = TokenRuntime::new(
        handler,
        sidecar,
        client,
        runtime_config.direct_registry.clone(),
        runtime_config.registry_client.clone(),
    );
    if let Some(cache_registry) = runtime_config.cache_registry.as_ref() {
        let cache: Arc<dyn RuntimeCache> = runtime.cache();
        cache_registry.register_arc(TOKEN_CACHE_NAME, cache);
    }
    Ok(Some(runtime))
}

pub(crate) fn load_client_config(
    runtime_config: &RuntimeConfig,
) -> Result<ClientTokenConfig, RuntimeError> {
    if let Some(client) = runtime_config.client.as_ref() {
        return Ok(client.clone());
    }

    let client = runtime_config
        .module_registry
        .load_config::<ClientTokenConfig>(runtime_config, CLIENT_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        CLIENT_TOKEN_MODULE_ID,
        CLIENT_TOKEN_CONFIG_NAME,
        ModuleKind::Core,
        &client,
        client_config_masks(),
        true,
        Some(true),
        true,
    )?;
    Ok(client)
}

pub async fn apply_token_request(
    session: &mut Session,
    runtime: &TokenRuntime,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    let request_is_http = session_request_is_http(session);
    gateway_policy::token::apply_token_request(
        &mut PolicyRequest(session),
        runtime,
        request_path,
        request_is_http,
    )
    .await
}

fn session_request_is_http(session: &Session) -> bool {
    session
        .as_downstream()
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_none()
}

#[cfg(test)]
mod tests {
    #[test]
    fn client_token_config_accepts_single_and_multiple_auth_server_shapes() {
        gateway_policy::token::tests::client_token_config_accepts_single_and_multiple_auth_server_shapes();
    }

    #[test]
    fn sidecar_config_defaults_to_header_mode_and_supports_protocol_mode() {
        gateway_policy::token::tests::sidecar_config_defaults_to_header_mode_and_supports_protocol_mode();
    }

    #[test]
    fn token_handler_applies_only_to_boundary_prefixes() {
        gateway_policy::token::tests::token_handler_applies_only_to_boundary_prefixes();
    }

    #[test]
    fn client_credentials_form_and_authorization_are_java_compatible() {
        gateway_policy::token::tests::client_credentials_form_and_authorization_are_java_compatible(
        );
    }

    #[test]
    fn jwt_exp_claim_is_used_for_cache_expiry() {
        gateway_policy::token::tests::jwt_exp_claim_is_used_for_cache_expiry();
    }

    #[tokio::test]
    async fn token_cache_masks_tokens_in_summary_and_evicts_shortest_expiry() {
        gateway_policy::token::tests::token_cache_masks_tokens_in_summary_and_evicts_shortest_expiry().await;
    }

    #[tokio::test]
    async fn expired_token_refresh_is_synchronized_for_same_cache_key() {
        gateway_policy::token::tests::expired_token_refresh_is_synchronized_for_same_cache_key()
            .await;
    }

    #[tokio::test]
    async fn early_refresh_returns_current_token_and_runs_once_in_background() {
        gateway_policy::token::tests::early_refresh_returns_current_token_and_runs_once_in_background().await;
    }

    #[tokio::test]
    async fn token_service_id_uses_direct_registry_without_portal_registry() {
        gateway_policy::token::tests::token_service_id_uses_direct_registry_without_portal_registry().await;
    }
}
