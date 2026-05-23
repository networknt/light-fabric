use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const CONFIG_DIR: &str = "config";
const OUTPUT_FILE: &str = "embedded_config.rs";

pub fn generate() -> io::Result<()> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(io::Error::other)?);
    let config_dir = manifest_dir.join(CONFIG_DIR);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(io::Error::other)?);
    let output_path = out_dir.join(OUTPUT_FILE);

    println!("cargo:rerun-if-changed={CONFIG_DIR}");

    let mut files = Vec::new();
    if config_dir.exists() {
        collect_config_files(&config_dir, &config_dir, &mut files)?;
    }
    files.sort();

    let mut output = String::from("pub const FILES: &[config_loader::EmbeddedConfigFile] = &[\n");
    for file in files {
        let display = file.to_string_lossy().replace('\\', "/");
        println!("cargo:rerun-if-changed={CONFIG_DIR}/{display}");
        output.push_str("    config_loader::EmbeddedConfigFile {\n");
        output.push_str(&format!("        name: {display:?},\n"));
        output.push_str(&format!(
            "        content: include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{CONFIG_DIR}/{display}\")),\n"
        ));
        output.push_str("    },\n");
    }
    output.push_str("];\n");

    fs::write(output_path, output)
}

fn collect_config_files(base_dir: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_config_files(base_dir, &path, files)?;
            continue;
        }
        if !is_supported_config(&path) {
            continue;
        }
        let relative = path
            .strip_prefix(base_dir)
            .map_err(io::Error::other)?
            .to_path_buf();
        files.push(relative);
    }

    Ok(())
}

fn is_supported_config(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("yml" | "yaml" | "json" | "toml")
    )
}
