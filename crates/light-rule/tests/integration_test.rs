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
    registry.register("mock-action", Arc::new(MockAction));
    let engine = Arc::new(RuleEngine::new(Arc::new(registry)));

    // 2. Build Rules
    let rule_access = Rule {
        rule_id: "rule_access_01".into(),
        rule_name: "Admin access".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: Some("Check if user is admin".into()),
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("standard".into()),
        access_control_effect: None,
        expression: Some("role == 'admin'".into()),
        conditions: None,
        actions: Some(vec![RuleAction {
            action_id: None,
            action_desc: None,
            action_ref: "mock-action".into(),
            action_values: Some(json!({"message": "Access Granted"})),
        }]),
    };

    let rule_bad_access = Rule {
        rule_id: "rule_access_02".into(),
        rule_name: "Verified client access".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: Some("Check if client is verified".into()),
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("standard".into()),
        access_control_effect: None,
        expression: Some("client_status == 'verified'".into()),
        conditions: None,
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
async fn test_legacy_native_conditions_are_rejected() {
    let mut registry = ActionRegistry::new();
    registry.register("mock-action", Arc::new(MockAction));
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_typed_01".into(),
        rule_name: "Typed condition operators".into(),
        rule_type: "access-control".into(),
        common: "N".into(),
        host_id: Some("host-01".into()),
        rule_desc: Some("Check typed condition operators".into()),
        version: Some("1.1.0".into()),
        author: Some("test".into()),
        updated_at: None,
        condition_language: None,
        condition_security_profile: None,
        access_control_effect: None,
        expression: None,
        conditions: Some(vec![
            RuleCondition {
                condition_id: Some("age".into()),
                condition_desc: Some("User must be adult".into()),
                operator: Some(">=".into()),
                operand: Some("user.age".into()),
                expected: Some(json!(18)),
                join_code: None,
            },
            RuleCondition {
                condition_id: Some("active".into()),
                condition_desc: None,
                operator: Some("==".into()),
                operand: Some("/user/active".into()),
                expected: Some(json!(true)),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("roles".into()),
                condition_desc: None,
                operator: Some("containsAll".into()),
                operand: Some("$.user.roles".into()),
                expected: Some(json!(["admin", "user"])),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("scope".into()),
                condition_desc: None,
                operator: Some("containsAny".into()),
                operand: Some("$.user.scopes".into()),
                expected: Some(json!(["portal.r", "portal.w"])),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("blocked-scope".into()),
                condition_desc: None,
                operator: Some("containsNone".into()),
                operand: Some("$.user.scopes".into()),
                expected: Some(json!(["admin.delete"])),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("email".into()),
                condition_desc: None,
                operator: Some("endsWith".into()),
                operand: Some("user.email".into()),
                expected: Some(json!("@lightapi.net")),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("name-length".into()),
                condition_desc: None,
                operator: Some("lengthGreaterThan".into()),
                operand: Some("user.name".into()),
                expected: Some(json!(10)),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("status-list".into()),
                condition_desc: None,
                operator: Some("inList".into()),
                operand: Some("user.status".into()),
                expected: Some(json!(["active", "pending"])),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("tier-list-json-string".into()),
                condition_desc: None,
                operator: Some("inList".into()),
                operand: Some("user.tier".into()),
                expected: Some(json!("[\"gold\",\"platinum\"]")),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("name-case".into()),
                condition_desc: None,
                operator: Some("containsIgnoreCase".into()),
                operand: Some("user.name".into()),
                expected: Some(json!("HU")),
                join_code: Some("AND".into()),
            },
            RuleCondition {
                condition_id: Some("missing".into()),
                condition_desc: None,
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
            "name": "Steve Hu Test",
            "status": "active",
            "tier": "gold",
            "roles": ["admin", "user"],
            "scopes": ["portal.r"],
            "email": "steve.hu@lightapi.net"
        }
    });

    let error = engine.execute_rule(&rule, &mut context).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Unsupported conditionLanguage: native")
    );
}

#[tokio::test]
async fn test_cel_replaces_join_code_or() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_join_01".into(),
        rule_name: "Join code OR".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: Some("Check left-to-right OR join".into()),
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("standard".into()),
        access_control_effect: None,
        expression: Some("role == 'admin' || role == 'operator'".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({ "role": "operator" });
    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_cel_condition_language() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_01".into(),
        rule_name: "CEL expression".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: Some("Check CEL expression".into()),
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("standard".into()),
        access_control_effect: None,
        expression: Some(
            "context.user.age >= 18 && contains_ignore_case(context.user.name, 'hu') && 'admin' in context.user.roles"
                .into(),
        ),
        conditions: Some(vec![RuleCondition {
            condition_id: Some("ignored-native-condition".into()),
            condition_desc: None,
            operator: Some("==".into()),
            operand: Some("user.age".into()),
            expected: Some(json!(0)),
            join_code: None,
        }]),
        actions: None,
    };

    let mut context = json!({
        "user": {
            "age": 21,
            "name": "Steve Hu",
            "roles": ["admin", "user"]
        }
    });
    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_strict_cel_profile_uses_curated_roots() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_01".into(),
        rule_name: "Strict CEL expression".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some(
            "auditInfo.subject_claims.ClaimsMap.role == 'admin' && contains_ignore_case(headers.owner, 'hu')"
                .into(),
        ),
        conditions: None,
        actions: None,
    };

    let mut context = json!({
        "auditInfo": {
            "subject_claims": {
                "ClaimsMap": {
                    "role": "admin"
                }
            }
        },
        "headers": {
            "owner": "Steve Hu"
        },
        "internalState": {
            "tenantSecret": "hidden"
        }
    });

    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_strict_cel_profile_rejects_context_alias() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_02".into(),
        rule_name: "Strict CEL rejects context".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: None,
        access_control_effect: None,
        expression: Some("context.user.age >= 18".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({ "user": { "age": 21 } });
    let error = engine.execute_rule(&rule, &mut context).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("strict profile does not expose full context"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_strict_cel_profile_rejects_regex() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_03".into(),
        rule_name: "Strict CEL rejects regex".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some("headers.path.matches('^/admin')".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({ "headers": { "path": "/admin/users" } });
    let error = engine.execute_rule(&rule, &mut context).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("strict profile does not allow regex"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_response_phase_caps_standard_cel_to_strict() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_res_01".into(),
        rule_name: "Response CEL expression".into(),
        rule_type: "res-tra".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("standard".into()),
        access_control_effect: None,
        expression: Some("context.responseBody.items.size() > 0".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({ "responseBody": { "items": [1, 2, 3] } });
    let error = engine.execute_rule(&rule, &mut context).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("strict profile does not expose full context"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_internal_admin_cel_profile_is_disabled_by_default() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_internal_01".into(),
        rule_name: "Internal admin CEL expression".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("internal-admin".into()),
        access_control_effect: None,
        expression: Some("context.user.age >= 18".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({ "user": { "age": 21 } });
    let error = engine.execute_rule(&rule, &mut context).await.unwrap_err();
    assert!(
        error.to_string().contains("internal-admin is disabled"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_strict_cel_profile_list_matching() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_list_01".into(),
        rule_name: "Strict CEL list membership".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some("'portal.w' in auditInfo.subject_claims.ClaimsMap.scp".into()),
        conditions: None,
        actions: None,
    };

    let mut context = json!({
        "auditInfo": {
            "subject_claims": {
                "ClaimsMap": {
                    "scp": ["portal.r", "portal.w"]
                }
            }
        }
    });

    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_strict_cel_profile_missing_scp() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_list_02".into(),
        rule_name: "Strict CEL list membership missing".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some(
            "'scp' in auditInfo.subject_claims.ClaimsMap && 'portal.w' in auditInfo.subject_claims.ClaimsMap.scp".into(),
        ),
        conditions: None,
        actions: None,
    };

    let mut context = json!({
        "auditInfo": {
            "subject_claims": {
                "ClaimsMap": {
                    // scp is missing
                }
            }
        }
    });

    let res = engine.execute_rule(&rule, &mut context).await.unwrap();
    assert!(
        !res,
        "Expected rule to evaluate to false when scp is missing"
    );
}

#[tokio::test]
async fn test_strict_cel_profile_endpoint_permission() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_list_03".into(),
        rule_name: "Strict CEL dynamic permission match".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some(
            "'scp' in auditInfo.subject_claims.ClaimsMap && permission.groups in auditInfo.subject_claims.ClaimsMap.scp".into(),
        ),
        conditions: None,
        actions: None,
    };

    let mut context = json!({
        "auditInfo": {
            "subject_claims": {
                "ClaimsMap": {
                    "scp": ["portal.r", "portal.w"]
                }
            }
        },
        "permission": {
            "groups": "portal.w"
        }
    });

    assert!(engine.execute_rule(&rule, &mut context).await.unwrap());
}

#[tokio::test]
async fn test_strict_cel_profile_missing_permission_variable() {
    let registry = ActionRegistry::new();
    let engine = RuleEngine::new(Arc::new(registry));

    let rule = Rule {
        rule_id: "rule_cel_strict_list_04".into(),
        rule_name: "Strict CEL missing permission var".into(),
        rule_type: "access-control".into(),
        common: "Y".into(),
        host_id: None,
        rule_desc: None,
        version: None,
        author: None,
        updated_at: None,
        condition_language: Some("cel".into()),
        condition_security_profile: Some("strict".into()),
        access_control_effect: None,
        expression: Some(
            "'scp' in auditInfo.subject_claims.ClaimsMap && 'groups' in permission && permission.groups in auditInfo.subject_claims.ClaimsMap.scp".into(),
        ),
        conditions: None,
        actions: None,
    };

    let mut context = json!({
        "auditInfo": {
            "subject_claims": {
                "ClaimsMap": {
                    "scp": ["portal.r", "portal.w"]
                }
            }
        }
        // permission is missing
    });

    let res = engine.execute_rule(&rule, &mut context).await.unwrap();
    assert!(!res);
}
