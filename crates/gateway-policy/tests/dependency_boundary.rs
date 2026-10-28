use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
};

#[test]
fn gateway_policy_closure_is_transport_neutral() {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: BTreeMap<_, _> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let edges: BTreeMap<_, _> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["id"].as_str().unwrap(),
                p["dependencies"].as_array().unwrap(),
            )
        })
        .collect();
    let root = names
        .iter()
        .find(|(_, name)| **name == "gateway-policy")
        .expect("gateway-policy is required")
        .0;
    let mut stack = vec![*root];
    let mut visited = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let name = names[id];
        assert!(!name.starts_with("pingora"), "transport dependency {name}");
        assert!(
            !name.starts_with("lambda_runtime"),
            "runtime dependency {name}"
        );
        assert!(
            !name.starts_with("aws-") || ["aws-lc-rs", "aws-lc-sys"].contains(&name),
            "service SDK dependency {name}"
        );
        assert!(
            ![
                "light-pingora",
                "light-gateway",
                "light-lambda",
                "light-lambda-runtime",
                "serverless-core",
                "serverless-aws"
            ]
            .contains(&name),
            "framework/app dependency {name}"
        );
        if let Some(deps) = edges.get(id) {
            stack.extend(deps.iter().map(|d| d.as_str().unwrap()));
        }
    }
    eprintln!(
        "gateway-policy: {} packages including build/dev closure",
        visited.len()
    );
}
