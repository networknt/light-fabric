//! Gateway-owned contract between the shipped policy and runtime image inputs.
use config_loader::ConfigLoader;
use light_pingora::SpaTokenTrustConfig;
use parse_dockerfile::{Instruction, Source};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

const CONTEXT_CA: &str = "apps/light-gateway/config/ca.pem";

// Trace the shipped CA through literal context COPY instructions. This checks
// the builder layout contract; it does not execute arbitrary RUN instructions.
fn builder_ca_path(parsed: &parse_dockerfile::Dockerfile<'_>) -> Result<PathBuf, String> {
    let builder = parsed
        .stages()
        .find(|stage| {
            stage
                .from
                .as_
                .as_ref()
                .is_some_and(|(_, alias)| alias.value == "builder")
        })
        .ok_or("gateway Dockerfile has no builder stage")?;
    let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ca = Path::new(CONTEXT_CA);
    if !context.join(ca).is_file() {
        return Err("shipped CA is missing from the workspace build context".into());
    }
    let mut workdir: Option<PathBuf> = None;
    let mut copied_ca = None;
    for instruction in builder.instructions {
        match instruction {
            Instruction::Workdir(dir) => {
                let path = Path::new(dir.arguments.value.as_ref());
                workdir = if path.is_absolute() {
                    Some(path.to_owned())
                } else {
                    workdir.map(|current| current.join(path))
                };
            }
            Instruction::Copy(copy)
                if !copy.options.iter().any(|flag| flag.name.value == "from") =>
            {
                for source in &copy.src {
                    let Source::Path(source) = source else {
                        continue;
                    };
                    let source = Path::new(source.value.as_ref());
                    let Ok(suffix) = ca.strip_prefix(source) else {
                        continue;
                    };
                    let destination = Path::new(copy.dest.value.as_ref());
                    let destination = if destination.is_absolute() {
                        destination.to_owned()
                    } else {
                        workdir
                            .as_ref()
                            .ok_or("CA COPY needs a known absolute builder WORKDIR")?
                            .join(destination)
                    };
                    copied_ca = if context.join(source).is_dir() {
                        Some(destination.join(suffix))
                    } else if source == ca {
                        Some(if copy.dest.value.ends_with('/') || copy.src.len() > 1 {
                            destination.join(ca.file_name().unwrap())
                        } else {
                            destination
                        })
                    } else {
                        None
                    };
                }
            }
            _ => {}
        }
    }
    copied_ca.ok_or_else(|| format!("builder does not COPY {CONTEXT_CA} from the build context"))
}

fn check_ca_copy(dockerfile: &str, destination: &str) -> Result<(), String> {
    let parsed = parse_dockerfile::parse(dockerfile)
        .map_err(|error| format!("invalid gateway Dockerfile: {error}"))?;
    let expected_source = builder_ca_path(&parsed)?;
    let runtime = parsed
        .stages()
        .last()
        .ok_or("gateway image has no runtime stage")?;
    let copy = runtime
        .instructions
        .iter()
        .rev()
        .find_map(|instruction| match instruction {
            Instruction::Copy(copy) if copy.dest.value == destination => Some(copy),
            _ => None,
        })
        .ok_or_else(|| format!("runtime stage must COPY the shipped CA to {destination}"))?;
    let from_builder = copy.options.iter().any(|flag| {
        flag.name.value == "from"
            && flag
                .value
                .as_ref()
                .is_some_and(|value| value.value == "builder")
    });
    if !from_builder
        || copy.src.len() != 1
        || !matches!(&copy.src[0], Source::Path(path) if Path::new(path.value.as_ref()) == expected_source)
    {
        return Err(format!(
            "{destination} must come from builder:{}",
            expected_source.display()
        ));
    }
    Ok(())
}

fn fixture_dockerfile(runtime_copy: &str) -> String {
    format!(
        "FROM scratch AS builder\nWORKDIR /usr/src/app\nCOPY apps/ apps/\nFROM scratch\n{runtime_copy}\n"
    )
}

#[test]
fn runtime_image_copies_shipped_ca_to_the_templates_default_path() {
    let app = Path::new(env!("CARGO_MANIFEST_DIR"));
    let loader = ConfigLoader::from_values(HashMap::new(), None, None).unwrap();
    let policy: SpaTokenTrustConfig = loader
        .load_typed([app.join("config/spa-token-trust.yml")])
        .unwrap();
    let [destination] = policy.internal.jwks_ca_paths.as_slice() else {
        panic!("gateway template must name its single shipped CA bundle");
    };
    check_ca_copy(include_str!("../docker/Dockerfile"), destination).unwrap();
    assert!(
        !reqwest::Certificate::from_pem_bundle(include_bytes!("../config/ca.pem"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ca_copy_contract_accepts_multiline_comments_and_json_form() {
    let ca_source = format!("/usr/src/app/{CONTEXT_CA}");
    for instruction in [
        format!(
            r#"COPY --from=builder \
            # the shipped development CA
            {ca_source} \
            /config/ca.pem"#
        ),
        format!("COPY --from=builder [\"{ca_source}\", \"/config/ca.pem\"]"),
    ] {
        let dockerfile = fixture_dockerfile(&instruction);
        check_ca_copy(&dockerfile, "/config/ca.pem").unwrap();
    }
}

#[test]
fn ca_copy_contract_rejects_wrong_source_stage_and_destination() {
    let ca_source = format!("/usr/src/app/{CONTEXT_CA}");
    for instruction in [
        "COPY --from=builder /usr/src/app/other-ca.pem /config/ca.pem".to_owned(),
        format!("COPY --from=unrelated {ca_source} /config/ca.pem"),
        format!("COPY --from=builder {ca_source} /config/other-ca.pem"),
    ] {
        let dockerfile = fixture_dockerfile(&instruction);
        assert!(check_ca_copy(&dockerfile, "/config/ca.pem").is_err());
    }
    // A matching COPY in an intermediate stage does not package the runtime.
    let dockerfile = format!(
        "FROM scratch AS builder\nWORKDIR /usr/src/app\nCOPY apps/ apps/\nFROM scratch AS intermediate\nCOPY --from=builder {ca_source} /config/ca.pem\nFROM scratch\n"
    );
    assert!(check_ca_copy(&dockerfile, "/config/ca.pem").is_err());
}

#[test]
fn ca_copy_contract_tracks_builder_layout_changes() {
    let dockerfile = include_str!("../docker/Dockerfile");
    // A coordinated relocation remains valid without changing the test's paths.
    let relocated = dockerfile.replace("/usr/src/app", "/build/context");
    check_ca_copy(&relocated, "/config/ca.pem").unwrap();
    // Moving only the builder root leaves the runtime COPY source stale.
    let stale = dockerfile.replacen("WORKDIR /usr/src/app", "WORKDIR /build/context", 1);
    assert!(check_ca_copy(&stale, "/config/ca.pem").is_err());
    let source = format!("COPY --from=builder /usr/src/app/{CONTEXT_CA} /config/ca.pem");
    let missing = fixture_dockerfile(&source).replace("COPY apps/ apps/\n", "");
    assert!(check_ca_copy(&missing, "/config/ca.pem").is_err());
    let misplaced = fixture_dockerfile(&source).replace("COPY apps/ apps/", "COPY apps/ wrong/");
    assert!(check_ca_copy(&misplaced, "/config/ca.pem").is_err());
}
