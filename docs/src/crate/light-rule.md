# Light Rule

`light-rule` is the Rust rule engine for evaluating rule definitions and
executing registered actions.

It is designed to align with the `rule.yaml` specification while remaining
runtime-neutral. Java services can use `yaml-rule`; Rust services use this
crate.

## Main Types

- `RuleEngine`: evaluates rule conditions and determines action execution.
- `MultiThreadRuleExecutor`: executes rules with runtime state.
- `RuntimeState`: input/output state passed through rule evaluation.
- `ActionRegistry`: registry for action plugins.
- `RuleActionPlugin`: trait implemented by Rust action handlers.
- `Rule`, `RuleCondition`, `RuleAction`, `RuleConfig`, `EndpointConfig`: rule
  model types.

## Action Model

Rules reference actions by `actionRef`. In Rust, `actionRef` resolves to a
registered `RuleActionPlugin`; it is not a Java class name. This keeps the rule
format portable across Java and Rust executors.

## Usage

```rust
use light_rule::{ActionRegistry, RuleEngine};

let registry = ActionRegistry::default();
let engine = RuleEngine::new(registry);
```

## Related Design

See [Light-Rule](../design/light-rule.md) for the rule format and its
relationship to workflow assertions and portal rule management.
