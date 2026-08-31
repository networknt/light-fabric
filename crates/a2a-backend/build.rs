use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() {
    let contract = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/a2a-backend/v1/openapi.yaml");
    let bytes = std::fs::read(&contract).expect("read canonical A2A backend OpenAPI contract");
    println!("cargo:rerun-if-changed={}", contract.display());
    println!(
        "cargo:rustc-env=A2A_BACKEND_CONTRACT_DIGEST=sha256:{:x}",
        Sha256::digest(bytes)
    );
}
