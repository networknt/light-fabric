use crate::models::{Rule, RuleConfig};
use crate::engine::RuleEngine;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, debug};

/// State of the executor, containing the rules and endpoints mappings.
pub struct RuntimeState {
    pub endpoint_rules: HashMap<String, crate::models::EndpointConfig>,
    pub rules: HashMap<String, Rule>,
}

/// The multi-threaded Executor capable of running rules sequentially or in parallel.
pub struct MultiThreadRuleExecutor {
    state: Arc<RuntimeState>,
    engine: Arc<RuleEngine>,
}

impl MultiThreadRuleExecutor {
    pub fn new(config: RuleConfig, engine: Arc<RuleEngine>) -> Self {
        let state = RuntimeState {
            endpoint_rules: config.endpoint_rules.clone(),
            rules: config.rule_bodies.clone(),
        };

        Self {
            state: Arc::new(state),
            engine,
        }
    }

    /// Execute a single rule by its ID.
    pub async fn execute_rule(&self, rule_id: &str, input: &mut Value) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(rule) = self.state.rules.get(rule_id) {
            self.engine.execute_rule(rule, input).await
        } else {
            error!("Rule not found: {}", rule_id);
            Ok(false)
        }
    }

    /// Execute multiple rules using a given logic ("all", "any", "parallel").
    pub async fn execute_rules(&self, rule_ids: &[String], logic: &str, input: &mut Value) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if rule_ids.is_empty() {
            return Ok(true);
        }

        if logic.eq_ignore_ascii_case("all") {
            // Sequential ALL
            for rule_id in rule_ids {
                let res = self.execute_rule(rule_id, input).await?;
                if !res {
                    return Ok(false);
                }
            }
            Ok(true)
        } else if logic.eq_ignore_ascii_case("any") {
            // Sequential ANY
            for rule_id in rule_ids {
                let res = self.execute_rule(rule_id, input).await?;
                if res {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            // Parallel (default)
            // To run parallel mutating exactly the same `input` is risky in Rust due to borrowing rules.
            // In Java, `Map<String, Object>` is shared and concurrently mutated (often unsafely).
            // Here, we clone the input for each task, and if they pass, we return true.
            // Mutating concurrently requires a Mutex. For parallel validation (which usually shouldn't mutate), clones are fine.
            let mut handles = vec![];

            for rule_id in rule_ids {
                let engine_clone = Arc::clone(&self.engine);
                let rule_id_clone = rule_id.clone();
                let rule_clone = self.state.rules.get(&rule_id_clone).cloned();
                let mut input_clone = input.clone();

                handles.push(tokio::spawn(async move {
                    if let Some(rule) = rule_clone {
                        engine_clone.execute_rule(&rule, &mut input_clone).await
                    } else {
                        Ok(false)
                    }
                }));
            }

            let results = futures::future::join_all(handles).await;
            
            let mut any_failed = false;
            for res in results {
                match res {
                    Ok(Ok(true)) => {}, // Rule passed
                    _ => { any_failed = true; } // Join error or rule failed
                }
            }

            Ok(!any_failed)
        }
    }

    /// Execute rules configured for a specific service entry and rule type.
    pub async fn execute_endpoint_rules(&self, service_entry: &str, rule_type: &str, input: &mut Value) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint_config = match self.state.endpoint_rules.get(service_entry) {
            Some(c) => c,
            None => {
                debug!("No rules found for serviceEntry: {}", service_entry);
                return Ok(true);
            }
        };

        let crate::models::EndpointConfig::Map(map) = endpoint_config;
        if let Some(rule_ids_val) = map.get(rule_type) {
            if let Some(rule_ids_arr) = rule_ids_val.as_array() {
                let rule_ids: Vec<String> = rule_ids_arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();

                // Apply permissions if present
                if let Some(perm) = map.get("permission") {
                    if let Value::Object(perm_map) = perm {
                        if let Value::Object(input_map) = input {
                            for (k, v) in perm_map {
                                input_map.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }

                let logic = if rule_type == "req-tra" || rule_type == "res-tra" {
                    "all"
                } else {
                    "parallel"
                };

                return self.execute_rules(&rule_ids, logic, input).await;
            }
        }
        
        Ok(true)
    }
}
