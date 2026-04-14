use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// A trait that mimics the `IAction` interface in Java's yaml-rule-plugin.
#[async_trait]
pub trait RuleActionPlugin: Send + Sync {
    /// Executes the action. It may mutate the `rule_context` map in place.
    /// Returns a boolean indicating success of the action.
    async fn execute(&self, rule_context: &mut Value, action_values: &Option<Value>) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

/// A registry that holds mapping from `actionClassName` strings to boxed `RuleActionPlugin` structs.
pub struct ActionRegistry {
    plugins: HashMap<String, Arc<dyn RuleActionPlugin>>,
}

impl ActionRegistry {
    /// Create a new Action Registry
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
        }
    }

    /// Register a plugin with its string identifier.
    pub fn register<S: Into<String>>(&mut self, class_name: S, plugin: Arc<dyn RuleActionPlugin>) {
        self.plugins.insert(class_name.into(), plugin);
    }

    /// Retrieve an action plugin by class name.
    pub fn get(&self, class_name: &str) -> Option<Arc<dyn RuleActionPlugin>> {
        self.plugins.get(class_name).cloned()
    }
}
