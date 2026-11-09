//! Verify a built image: cargo run -p light-gateway --example image_ca_smoke -- IMAGE
use anyhow::{Context, bail, ensure};
use config_loader::ConfigLoader;
use light_pingora::SpaTokenTrustConfig;
use std::{collections::HashMap, path::Path, process::Command};

fn main() -> anyhow::Result<()> {
    let image = std::env::args()
        .nth(1)
        .context("supply a locally built gateway image")?;
    let loader = ConfigLoader::from_values(HashMap::new(), None, None)?;
    let policy: SpaTokenTrustConfig = loader
        .load_typed([Path::new(env!("CARGO_MANIFEST_DIR")).join("config/spa-token-trust.yml")])?;
    let [ca_path] = policy.internal.jwks_ca_paths.as_slice() else {
        bail!("gateway template must specify its single development CA bundle");
    };
    // Do not start the gateway, access the network, mount host files, or change
    // the image USER: the packaged CA must be readable by that configured user.
    let output = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--pull=never",
            "--network",
            "none",
            "--read-only",
            "--entrypoint",
            "/bin/cat",
            &image,
            ca_path,
        ])
        .output()
        .context("run gateway image CA smoke check")?;
    ensure!(
        output.status.success(),
        "cannot read {ca_path} from {image}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.stdout == include_bytes!("../config/ca.pem"),
        "CA bytes at {ca_path} in {image} differ from the shipped development CA"
    );
    println!("Gateway image CA matches the shipped development CA at {ca_path}");
    Ok(())
}
