//! Bounded, fixture-local scheduling witnesses. Never request data or SDK-wire proof.
use serde_json::{Value, json};
use std::{sync::Mutex, time::Instant};

#[derive(Default)]
pub(crate) struct Timeline(Mutex<Option<Window>>);
struct Window {
    start: Instant,
    budget_ms: u64,
    events: Vec<(&'static str, u64)>,
}
impl Timeline {
    pub fn begin(&self, budget_ms: u64) {
        *self.0.lock().unwrap() = Some(Window {
            start: Instant::now(),
            budget_ms,
            events: Vec::new(),
        });
    }
    pub fn record(&self, phase: &'static str) {
        if let Some(window) = &mut *self.0.lock().unwrap()
            && window.events.len() < 8
        {
            window
                .events
                .push((phase, window.start.elapsed().as_micros() as u64));
        }
    }
    pub fn observation(&self, step: usize) -> Value {
        let window = self.0.lock().unwrap();
        let window = window.as_ref().expect("started fixture timing window");
        json!({"step":step,"budgetMs":window.budget_ms,"events":window.events.iter().map(|(phase,elapsed)|json!({"phase":phase,"elapsedUs":elapsed})).collect::<Vec<_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_rejects_payloads_unknown_phases_and_unbounded_records() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../tests/serverless-conformance/schema/result-v1.schema.json"
        ))
        .unwrap();
        let validator =
            jsonschema::validator_for(&schema["properties"]["boundedWorkTiming"]).unwrap();
        let timing = Timeline::default();
        timing.begin(50);
        timing.record("service-start");
        timing.record("invoke-received");
        let valid = json!([timing.observation(0)]);
        assert!(validator.is_valid(&valid));
        for (pointer, value) in [
            ("/0/events/0/phase", json!("authorization")),
            ("/0/events/0/elapsedUs", json!(-1)),
            ("/0/budgetMs", json!(900001)),
            ("/0/events/0/body", json!("synthetic-secret")),
        ] {
            let mut invalid = valid.clone();
            if pointer.ends_with("/body") {
                invalid[0]["events"][0]["body"] = value;
            } else {
                *invalid.pointer_mut(pointer).unwrap() = value;
            }
            assert!(!validator.is_valid(&invalid), "{pointer}");
        }
        let mut invalid = valid.clone();
        invalid[0]["events"] = json!(vec![valid[0]["events"][0].clone(); 9]);
        assert!(!validator.is_valid(&invalid));
    }

    #[test]
    fn windows_are_bounded_fresh_and_independent() {
        let first = Timeline::default();
        let other = Timeline::default();
        first.record("response-enter"); // Unstarted windows record nothing.
        first.begin(50);
        other.begin(200);
        for _ in 0..20 {
            first.record("invoke-received");
        }
        let witness = first.observation(0);
        assert_eq!(witness["events"].as_array().unwrap().len(), 8);
        assert!(
            other.observation(0)["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        first.begin(75);
        first.record("service-return");
        let fresh = first.observation(1);
        assert_eq!(fresh["budgetMs"], 75);
        assert_eq!(fresh["events"].as_array().unwrap().len(), 1);
        assert_eq!(fresh["events"][0]["phase"], "service-return");
        assert_eq!(fresh.as_object().unwrap().len(), 3);
        assert_eq!(fresh["events"][0].as_object().unwrap().len(), 2);
    }
}
