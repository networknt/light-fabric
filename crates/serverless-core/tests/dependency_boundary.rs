use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
};
#[test]
fn dependency_closures_obey_provider_and_transport_boundaries() {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: BTreeMap<_, _> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let edges: BTreeMap<_, _> = meta["resolve"]["nodes"]
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
    for (root, required) in [
        ("serverless-core", true),
        ("gateway-policy", false),
        ("serverless-aws", false),
        ("light-lambda-runtime", false),
        ("light-lambda", false),
    ] {
        let Some((&id, _)) = names.iter().find(|(_, name)| **name == root) else {
            assert!(!required, "required crate absent");
            eprintln!("DEFERRED: {root} does not yet exist; no boundary execution claimed");
            continue;
        };
        let mut stack = vec![id];
        let mut visited = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue;
            }
            let name = names[id];
            assert!(
                !name.starts_with("pingora"),
                "{root} transitively depends on {name}"
            );
            if ["serverless-core", "gateway-policy"].contains(&root) {
                assert!(
                    allowed_in_core(name),
                    "{root} transitively depends on {name}"
                );
            }
            if root == "serverless-aws" {
                assert_ne!(name, "lambda_runtime");
            }
            if let Some(deps) = edges.get(id) {
                stack.extend(deps.iter().map(|v| v.as_str().unwrap()));
            }
        }
        eprintln!(
            "CHECKED {root}: {} packages in resolved dependency closure (including build/dev edges)",
            visited.len()
        );
    }
}

fn allowed_in_core(name: &str) -> bool {
    (!name.starts_with("aws-") || ["aws-lc-rs", "aws-lc-sys"].contains(&name))
        && !name.starts_with("lambda_runtime")
        && name != "light-lambda-runtime"
        && name != "serverless-aws"
}

#[test]
fn cryptographic_names_do_not_exempt_service_sdks_or_runtime() {
    for name in ["aws-lc-rs", "aws-lc-sys", "jsonwebtoken"] {
        assert!(allowed_in_core(name), "{name}");
    }
    for name in [
        "aws-config",
        "aws-sdk-lambda",
        "aws-smithy-runtime",
        "aws-lc-other",
        "lambda_runtime",
        "lambda_runtime_api_client",
        "light-lambda-runtime",
        "serverless-aws",
    ] {
        assert!(!allowed_in_core(name), "{name}");
    }
}
