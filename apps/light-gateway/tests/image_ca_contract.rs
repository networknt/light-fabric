//! Gateway-owned contract between the shipped policy and runtime image inputs.
use config_loader::ConfigLoader;
use light_pingora::SpaTokenTrustConfig;
use parse_dockerfile::{Instruction, Source};
use std::{collections::HashMap, path::Path};

const CA_SOURCE: &str = "/usr/src/app/apps/light-gateway/config/ca.pem";

fn check_ca_copy(dockerfile: &str, destination: &str) -> Result<(), String> {
    let parsed = parse_dockerfile::parse(dockerfile)
        .map_err(|error| format!("invalid gateway Dockerfile: {error}"))?;
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
        || !matches!(&copy.src[0], Source::Path(path) if path.value == CA_SOURCE)
    {
        return Err(format!("{destination} must come from builder:{CA_SOURCE}"));
    }
    Ok(())
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
    for instruction in [
        format!(
            r#"COPY --from=builder \
            # the shipped development CA
            {CA_SOURCE} \
            /config/ca.pem"#
        ),
        format!("COPY --from=builder [\"{CA_SOURCE}\", \"/config/ca.pem\"]"),
    ] {
        let dockerfile = format!("FROM scratch AS builder\nFROM scratch\n{instruction}\n");
        check_ca_copy(&dockerfile, "/config/ca.pem").unwrap();
    }
}

#[test]
fn ca_copy_contract_rejects_wrong_source_stage_and_destination() {
    for instruction in [
        "COPY --from=builder /usr/src/app/other-ca.pem /config/ca.pem".to_owned(),
        format!("COPY --from=unrelated {CA_SOURCE} /config/ca.pem"),
        format!("COPY --from=builder {CA_SOURCE} /config/other-ca.pem"),
    ] {
        let dockerfile = format!("FROM scratch AS builder\nFROM scratch\n{instruction}\n");
        assert!(check_ca_copy(&dockerfile, "/config/ca.pem").is_err());
    }
    // A matching COPY in an intermediate stage does not package the runtime.
    let dockerfile = format!(
        "FROM scratch AS builder\nFROM scratch AS intermediate\nCOPY --from=builder {CA_SOURCE} /config/ca.pem\nFROM scratch\n"
    );
    assert!(check_ca_copy(&dockerfile, "/config/ca.pem").is_err());
}
