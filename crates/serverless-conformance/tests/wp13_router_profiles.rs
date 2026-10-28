use serde_json::json;
use serverless_conformance::{fixture_files, incremental, load};
use std::path::PathBuf;

#[tokio::test]
async fn router_component_and_shared_deadline_keep_both_init_blockers() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1");
    for id in [
        "H-04-already-failed-router-component-v1",
        "H-04-shared-http-deadline-multi-contract-v1",
    ] {
        let original = fixture_files(&root)
            .unwrap()
            .iter()
            .map(|p| load(p).unwrap())
            .find(|f| f["id"] == id)
            .unwrap();
        // Local admission probe only; it cannot count as the direct-handler fixture.
        let mut probe = original.clone();
        probe.as_object_mut().unwrap().remove("component");
        probe["expect"] = json!({"outcome":"response","status":200,"body":"local-admission-probe","backendCalls":[],"jwksFetches":0});
        let result = incremental::run_fixture(&probe, &"0".repeat(40))
            .await
            .unwrap();
        assert_eq!(result["execution"], "failed");
        assert!(
            result["assertionErrors"]
                .to_string()
                .contains("handler enabled flag required")
        );
        let mut activated = probe.clone();
        activated["config"]["files"]["router"]["enabled"] = json!(true);
        let result = incremental::run_fixture(&activated, &"0".repeat(40))
            .await
            .unwrap();
        assert_eq!(result["execution"], "failed");
        assert!(
            result["assertionErrors"]
                .to_string()
                .contains("Java rewrite execution is unavailable"),
            "{id}: {}",
            result["assertionErrors"]
        );
        assert_eq!(result["observations"][0]["outcome"], "initError");
        assert_eq!(result["observations"][0]["backendCalls"], json!([]));
        assert_eq!(result["observations"][0]["httpCalls"], json!([]));
        assert!(
            original["config"]["files"]["router"]
                .get("enabled")
                .is_none()
        );
    }
}
