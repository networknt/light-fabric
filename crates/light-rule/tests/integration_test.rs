use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

use light_rule::{
    ActionRegistry, EndpointConfig, MultiThreadRuleExecutor, Rule, RuleAction, RuleActionPlugin,
    RuleCondition, RuleConfig, RuleEngine,
};

struct MockAction;

#[async_trait]
impl RuleActionPlugin for MockAction {
    async fn execute(
        &self,
        rule_context: &mut Value,
        action_values: &Option<Value>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(vals) = action_values {
            if let Some(msg) = vals.get("message") {
                if let Value::Object(map) = rule_context {
                    map.insert("last_message".to_string(), msg.clone());
                }
            }
        }
        Ok(true)
    }
}

fn build_test_config() -> (RuleConfig, Arc<RuleEngine>) {
    // 1. Build Registry
    let mut registry = ActionRegistry::new();
    registry.register("com.networknt.rule.MockAction", Arc::new(MockAction));
    let engine = Arc::new(RuleEngine::new(Arc::new(registry)));

    // 2. Build Rules
    let rule_access = Rule {
        rule_id: "rule_access_01".into(),
        rule_desc: Some("Check if user is admin".into()),
        conditions: Some(vec![RuleCondition {
            condition_id: None,
            operator: Some("==".into()),
            operand: Some("role".into()),
            expected: Some("admin".into()),
            join_code: None,
        }]),
        actions: Some(vec![RuleAction {
            action_id: None,
            action_class_name: "com.networknt.rule.MockAction".into(),
            action_values: Some(json!({"message": "Access Granted"})),
        }]),
    };

    let rule_bad_access = Rule {
        rule_id: "rule_access_02".into(),
        rule_desc: Some("Check if client is verified".into()),
        conditions: Some(vec![RuleCondition {
            condition_id: None,
            operator: Some("==".into()),
            operand: Some("client_status".into()),
            expected: Some("verified".into()),
            join_code: None,
        }]),
        actions: None,
    };

    let mut rule_bodies = HashMap::new();
    rule_bodies.insert("rule_access_01".into(), rule_access);
    rule_bodies.insert("rule_access_02".into(), rule_bad_access);

    // 3. Build Endpoint Rules
    let mut endpoint_rules = HashMap::new();

    let mut api_map = HashMap::new();
    api_map.insert(
        "access-control".into(),
        json!(["rule_access_01", "rule_access_02"]),
    );

    endpoint_rules.insert("/api/test@get".into(), EndpointConfig::Map(api_map));

    let config = RuleConfig {
        rule_bodies,
        endpoint_rules,
    };

    (config, engine)
}

#[tokio::test]
async fn test_parallel_access_control_pass() {
    let (config, engine) = build_test_config();
    let executor = MultiThreadRuleExecutor::new(config, engine);

    // Provide context where both conditions pass
    let mut context = json!({
        "role": "admin",
        "client_status": "verified"
    });

    let result = executor
        .execute_endpoint_rules("/api/test@get", "access-control", &mut context)
        .await
        .unwrap();
    assert!(result, "Expected rules to pass");
}

#[tokio::test]
async fn test_parallel_access_control_fail() {
    let (config, engine) = build_test_config();
    let executor = MultiThreadRuleExecutor::new(config, engine);

    // Provide context where one rule fails
    let mut context = json!({
        "role": "user", // Should fail rule_access_01
        "client_status": "verified"
    });

    let result = executor
        .execute_endpoint_rules("/api/test@get", "access-control", &mut context)
        .await
        .unwrap();
    assert!(!result, "Expected to fail because role != admin");
}

#[tokio::test]
async fn test_sequential_logic_all() {
    let (config, engine) = build_test_config();
    let executor = MultiThreadRuleExecutor::new(config, engine);

    let mut context = json!({
        "role": "admin",
        "client_status": "verified"
    });

    // Execute sequentially (ANY rule means if ANY fail it stops, actually ALL logic stops on first fail)
    let result = executor
        .execute_rules(
            &["rule_access_01".into(), "rule_access_02".into()],
            "all",
            &mut context,
        )
        .await
        .unwrap();
    assert!(result, "Expected all to pass");

    // In sequential mode, `input` MUST be meaningfully mutated.
    // Let's verify `MockAction` mutated our original map.
    assert_eq!(
        context.get("last_message").unwrap().as_str().unwrap(),
        "Access Granted"
    );
}
