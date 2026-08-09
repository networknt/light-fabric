use light_gateway::model_provider_sidecar::{
    SidecarProfileRequest, generate_sidecar_bundle, write_sidecar_bundle,
};
use std::path::PathBuf;

fn main() -> Result<(), String> {
    let mut arguments = std::env::args_os().skip(1);
    let request_path = arguments.next().map(PathBuf::from).ok_or_else(|| {
        "usage: model-provider-sidecar-profile REQUEST.json OUTPUT_DIR".to_string()
    })?;
    let output = arguments.next().map(PathBuf::from).ok_or_else(|| {
        "usage: model-provider-sidecar-profile REQUEST.json OUTPUT_DIR".to_string()
    })?;
    if arguments.next().is_some() {
        return Err("usage: model-provider-sidecar-profile REQUEST.json OUTPUT_DIR".to_string());
    }
    let request: SidecarProfileRequest =
        serde_json::from_slice(&std::fs::read(request_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let bundle = generate_sidecar_bundle(&request)?;
    write_sidecar_bundle(&output, &bundle)
}
