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
        version: None,
        author: None,
        updated_at: None,
        conditions: Some(vec![RuleCondition {
            condition_id: None,
            operator: Some("==".into()),
            operand: Some("role".into()),
            expected: Some(json!("admin")),
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
        version: None,
        author: None,
        updated_at: None,
        conditions: Some(vec![RuleCondition {
            condition_id: None,
            operator: Some("==".into()),
            operand: Some("client_status".into()),
            expected: Some(json!("verified")),
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

#[tokio::test]
async fn test_typed_expected_values_and_expanded_operators() {
    let mut registry = ActionRegistry::new();
    registry.register("com.networknt.rule.MockAction", Arc::new(MockAction));
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_typed_01".into(),
        rule_desc: Some("Check typed condition operators".into()),
        version: Some("1.1.0".into()),
        author: Some("test".into()),
        updated_at: None,
        conditions: Some(vec![
            RuleCondition {
                condition_id: Some("age".into()),
                operator: Some(">=".into()),
                operand: Some("user.age".into()),
                expected: Some(json!(18)),
                join_code: None,
            },
            RuleCondition {
                condition_id: Some("active".into()),
                operator: Some("==".into()),
                operand: Some("/user/active".into()),
                expected: Some(json!(true)),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("roles".into()),
                operator: Some("contains".into()),
                operand: Some("$.user.roles".into()),
                expected: Some(json!("admin")),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("email".into()),
                operator: Some("endsWith".into()),
                operand: Some("user.email".into()),
                expected: Some(json!("@lightapi.net")),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("missing".into()),
                operator: Some("notExists".into()),
                operand: Some("user.deletedAt".into()),
                expected: None,
                join_code: Some("AND".into()),
            },
        ]),
        actions: None,
    };

    let mut context = json!({
        "user": {
            "age": 21,
            "active": true,
            "roles": ["admin", "user"],
            "email": "steve.hu@lightapi.net"
        }
    });

    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_join_code_left_to_right_or() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_join_01".into(),
        rule_desc: Some("Check left-to-right OR join".into()),
        version: None,
        author: None,
        updated_at: None,
        conditions: Some(vec![
            RuleCondition {
                condition_id: Some("first".into()),
                operator: Some("==".into()),
                operand: Some("role".into()),
                expected: Some(json!("admin")),
                join_code: None,
            },
            RuleCondition {
                condition_id: Some("second".into()),
                operator: Some("==".into()),
                operand: Some("role".into()),
                expected: Some(json!("operator")),
                join_code: Some("OR".into()),
            },
        ]),
        actions: None,
    };

    let mut context = json!({ "role": "operator" });
    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}
