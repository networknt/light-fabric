pub mod client;
pub mod protocol;

pub use client::{PortalRegistryClient, RegistryHandler};
pub use protocol::{
    JsonRpcMessage, RegistrationResponse, ServiceMetadataUpdate, ServiceRegistrationParams,
};

use std::collections::HashMap;

/// Helper to build the registration parameters for a service.
pub struct RegistrationBuilder {
    params: ServiceRegistrationParams,
}

impl RegistrationBuilder {
    pub fn new(service_id: &str, version: &str, protocol: &str, address: &str, port: u16) -> Self {
        Self {
            params: ServiceRegistrationParams {
                service_id: service_id.to_string(),
                version: version.to_string(),
                protocol: protocol.to_string(),
                address: address.to_string(),
                port,
                tags: HashMap::new(),
                env_tag: None,
                jwt: String::new(),
            },
        }
    }

    pub fn with_tag(mut self, key: &str, value: &str) -> Self {
        self.params.tags.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_env(mut self, env: &str) -> Self {
        self.params.env_tag = Some(env.to_string());
        self
    }

    pub fn with_jwt(mut self, jwt: &str) -> Self {
        self.params.jwt = jwt.to_string();
        self
    }

    pub fn build(self) -> ServiceRegistrationParams {
        self.params
    }
}
