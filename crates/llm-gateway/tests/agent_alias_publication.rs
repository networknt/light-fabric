use llm_gateway::config::AliasConfig;

#[test]
fn java_agent_alias_publication_preserves_canonical_principal() {
    // Exported by AgentGatewayPublicationContractTest using appendV4Routes.
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/agent-alias-v1.json")).unwrap();
    let alias: AliasConfig = serde_json::from_value(raw.clone()).unwrap();
    let yaml = serde_yaml::to_string(&alias).unwrap();
    let restored: AliasConfig = serde_yaml::from_str(&yaml).unwrap();
    assert!(restored.internal);
    assert_eq!(
        restored.bound_principal.as_deref(),
        Some("019d82bf-ab5e-791a-885c-d08aafa2b614")
    );
    assert_eq!(
        restored.bound_principal.as_deref(),
        raw["boundPrincipal"].as_str()
    );
    // Service-client identity must not silently replace the agent definition UUID.
    assert_ne!(
        restored.bound_principal.as_deref(),
        Some("019d8349-41c6-72ef-95c2-4428a40d0e49")
    );
}
