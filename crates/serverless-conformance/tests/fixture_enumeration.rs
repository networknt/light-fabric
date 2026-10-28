use serverless_conformance::{fixture_files, manifest};
use std::{fs, path::PathBuf};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn digest_ignores_only_reserved_generated_outputs_and_preserves_fixture_identity() {
    let root =
        Scratch(std::env::temp_dir().join(format!("wp3-enumeration-{}", std::process::id())));
    assert!(!root.0.exists(), "refuse to reuse an existing scratch path");
    fs::create_dir_all(root.0.join("e")).unwrap();
    fs::write(root.0.join("e/B.json"), b"{\"z\":2,\"a\":[1,2]}").unwrap();
    fs::write(root.0.join("e/A.json"), b"{\"a\":1}").unwrap();
    let first = manifest(&root.0).unwrap();
    fs::create_dir_all(root.0.join("results/java")).unwrap();
    fs::create_dir_all(root.0.join("generated")).unwrap();
    fs::write(root.0.join("results/java/A.json"), b"not fixture JSON").unwrap();
    fs::write(root.0.join("generated/manifest.json"), b"{}").unwrap();
    fs::write(root.0.join("DIGEST"), b"earlier digest").unwrap();
    assert_eq!(fixture_files(&root.0).unwrap().len(), 2);
    assert_eq!(manifest(&root.0).unwrap(), first);
    let paths: Vec<_> = first
        .lines()
        .filter_map(|line| line.split_once("  ").map(|(_, path)| path))
        .filter(|path| *path != "TOTAL")
        .collect();
    assert_eq!(paths, ["e/A.json", "e/B.json"]);
    fs::write(root.0.join("e/B.json"), b"{ \"a\": [1, 2], \"z\": 2 }").unwrap();
    assert_eq!(manifest(&root.0).unwrap(), first);
    fs::write(root.0.join("e/B.json"), b"{\"a\":[2,1],\"z\":2}").unwrap();
    assert_ne!(manifest(&root.0).unwrap(), first);
}
