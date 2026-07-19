pub mod domain;
pub mod skill_packages;

#[cfg(test)]
mod portal_llm_contract_tests {
    #[test]
    fn policy_only_agent_contract_resolves_one_alias_and_fails_ambiguous_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.fixture.json",
        );
        let fixture: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path).expect("read shared Portal LLM eligibility fixture"),
        )
        .expect("parse shared Portal LLM eligibility fixture");
        let cases = fixture["cases"].as_array().expect("eligibility cases");
        let policy = cases
            .iter()
            .find(|case| case["name"] == "policy-only-agent-default")
            .expect("policy-only contract case");
        assert_eq!(policy["response"]["resolutionStatus"], "RESOLVED");
        assert_eq!(policy["response"]["resolvedModel"], "policy-chat");
        assert!(
            cases
                .iter()
                .any(|case| case["response"]["resolutionStatus"] == "AMBIGUOUS_DEFAULT")
        );
        assert!(
            cases
                .iter()
                .any(|case| case["response"]["resolutionStatus"] == "NO_DEFAULT")
        );
    }
}
