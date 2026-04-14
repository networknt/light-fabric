use crate::models::{Rule, RuleCondition};
use crate::action::ActionRegistry;
use serde_json::Value;
use std::sync::Arc;
use tracing::{error, debug};

/// The core Rule Engine for evaluating conditions and executing actions.
pub struct RuleEngine {
    action_registry: Arc<ActionRegistry>,
}

impl RuleEngine {
    /// Create a new RuleEngine with the given ActionRegistry.
    pub fn new(action_registry: Arc<ActionRegistry>) -> Self {
        Self { action_registry }
    }

    /// Execute a single rule against a context Object (map).
    /// Returns true if rules passed and actions executed successfully, false otherwise.
    pub async fn execute_rule(&self, rule: &Rule, context: &mut Value) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        debug!("Executing rule {}", rule.rule_id);
        
        // 1. Evaluate Conditions
        if let Some(ref conditions) = rule.conditions {
            let mut all_passed = true;
            for cond in conditions {
                if !self.evaluate_condition(cond, context) {
                    // Logic allows for OR/AND in Java, for simplicity assume AND unless handled.
                    all_passed = false;
                    break;
                }
            }

            if !all_passed {
                debug!("Conditions failed for rule {}", rule.rule_id);
                return Ok(false);
            }
        }

        // 2. Execute Actions
        if let Some(ref actions) = rule.actions {
            for action in actions {
                let plugin_opt = self.action_registry.get(&action.action_class_name);
                if let Some(plugin) = plugin_opt {
                    match plugin.execute(context, &action.action_values).await {
                        Ok(res) => {
                            if !res {
                                debug!("Action {} returned false for rule {}", action.action_class_name, rule.rule_id);
                                return Ok(false);
                            }
                        }
                        Err(e) => {
                            error!("Error executing action {} in rule {}: {}", action.action_class_name, rule.rule_id, e);
                            return Err(e);
                        }
                    }
                } else {
                    error!("Action class not found in registry: {}", action.action_class_name);
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    fn evaluate_condition(&self, condition: &RuleCondition, context: &Value) -> bool {
        // Simplified condition evaluation
        let operator = condition.operator.as_deref().unwrap_or("==");
        let operand_path = condition.operand.as_deref().unwrap_or("");
        let expected = condition.expected.as_deref().unwrap_or("");

        // In a real environment, evaluate `operand` json path against `context`
        // For simplicity of this port scaffold, grab direct key.
        let actual_val = match context.get(operand_path) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) => "".to_string(),
            Some(v) => v.to_string(),
            None => "".to_string(),
        };

        match operator {
            "==" | "eq" => actual_val == expected,
            "!=" | "ne" => actual_val != expected,
            ">" => {
                if let (Ok(a), Ok(b)) = (actual_val.parse::<f64>(), expected.parse::<f64>()) {
                    a > b
                } else { false }
            }
            "<" => {
                if let (Ok(a), Ok(b)) = (actual_val.parse::<f64>(), expected.parse::<f64>()) {
                    a < b
                } else { false }
            }
            _ => {
                error!("Unknown operator: {}", operator);
                false
            }
        }
    }
}
