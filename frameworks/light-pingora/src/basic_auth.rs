use crate::{policy_adapter::PolicyRequestRef, security::HandlerRejection};
pub use gateway_policy::basic_auth::*;
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
pub fn load_basic_auth_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<BasicAuthConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<BasicAuthConfig>(runtime_config, BASIC_AUTH_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == BASIC_AUTH_FILE => {
            BasicAuthConfig::default()
        }
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        BASIC_AUTH_MODULE_ID,
        BASIC_AUTH_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("password")],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn verify_basic_auth(
    session: &Session,
    config: &BasicAuthConfig,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    gateway_policy::basic_auth::verify_basic_auth(&PolicyRequestRef(session), config, request_path)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_users_accept_map_and_list_forms() {
        let map_config: BasicAuthConfig = serde_yaml::from_str(
            r#"
enabled: true
users:
  alice:
    password: secret
    paths:
      - /api
  anonymous:
    paths:
      - /public
"#,
        )
        .expect("parse map config");
        assert_eq!(map_config.users[0].username, "alice");
        assert_eq!(map_config.users[0].paths, ["/api"]);

        let list_config: BasicAuthConfig = serde_yaml::from_str(
            r#"
enabled: true
users: '[{"username":"bob","password":"secret","paths":["/v1"]}]'
"#,
        )
        .expect("parse list config");
        assert_eq!(list_config.users[0].username, "bob");
    }

    #[test]
    fn paths_default_to_all() {
        assert!(paths_allow(&[], "/anything"));
        assert!(paths_allow(&["/api".to_string()], "/api/pets"));
        assert!(!paths_allow(&["/api".to_string()], "/admin"));
    }

    #[test]
    fn blank_basic_password_never_authenticates() {
        let config: BasicAuthConfig = serde_yaml::from_str(
            "enabled: true\nusers:\n  - username: alice\n  - username: bob\n    password: ''\n",
        )
        .expect("parse users with missing and empty passwords");
        assert!(
            config
                .users
                .iter()
                .all(|user| !password_matches("", &user.password))
        );
        assert!(
            config
                .users
                .iter()
                .all(|user| !password_matches("secret", &user.password))
        );
        assert!(password_matches("secret", "secret"));
        assert!(!password_matches("wrong", "secret"));
    }
}
