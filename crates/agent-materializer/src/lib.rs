use agent_runtime_protocol::canonical_digest;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProductProfile {
    Enterprise,
    NativeWorkflow,
    Coding,
    PersonalAssistant,
    ExternalAdapter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstructionAuthority {
    Generated,
    Repository,
    Administrator,
    Product,
    Platform,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageCandidate {
    pub package_id: Uuid,
    pub name: String,
    pub version: String,
    pub product_profile: ProductProfile,
    pub content_digest: String,
    pub object_reference: String,
    pub entrypoint: String,
    pub authority: InstructionAuthority,
    pub compatibility: BTreeSet<String>,
    pub published: bool,
    pub revoked: bool,
    pub signer_verified: bool,
    pub scanner_approved: bool,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MaterializedPackage {
    pub package_id: Uuid,
    pub name: String,
    pub version: String,
    pub content_digest: String,
    pub object_reference: String,
    pub mount_target: String,
    pub entrypoint: String,
    pub authority: InstructionAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MaterializationManifest {
    pub schema_version: u16,
    pub materializer_id: String,
    pub materializer_version: u32,
    pub product_profile: ProductProfile,
    pub runtime_compatibility: String,
    pub packages: Vec<MaterializedPackage>,
    pub effective_instructions: Vec<String>,
    pub allowed_tools: BTreeSet<Uuid>,
    pub writable_roots: BTreeSet<String>,
}

impl MaterializationManifest {
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        canonical_digest(self)
    }
}

#[derive(Debug, Clone)]
pub struct MaterializationRequest {
    pub profile: ProductProfile,
    pub runtime_compatibility: String,
    pub allowed_package_ids: BTreeSet<Uuid>,
    pub allowed_tools: BTreeSet<Uuid>,
    pub writable_roots: BTreeSet<String>,
    pub candidates: Vec<PackageCandidate>,
}

pub fn materialize(
    request: MaterializationRequest,
) -> Result<MaterializationManifest, MaterializeError> {
    if request
        .writable_roots
        .iter()
        .any(|p| !p.starts_with("/workspace/") || p.contains(".."))
    {
        return Err(MaterializeError::WritableRoot);
    }
    let mut by_name = BTreeMap::new();
    for package in request.candidates {
        if !request.allowed_package_ids.contains(&package.package_id) {
            continue;
        }
        if package.product_profile != request.profile || !package.published || package.revoked {
            return Err(MaterializeError::Unavailable(package.name));
        }
        if !package.signer_verified || !package.scanner_approved {
            return Err(MaterializeError::Trust(package.name));
        }
        if !package
            .compatibility
            .contains(&request.runtime_compatibility)
        {
            return Err(MaterializeError::Compatibility(package.name));
        }
        if package.authority <= InstructionAuthority::Repository
            && contains_privilege_directive(&package.instructions)
        {
            return Err(MaterializeError::Privilege(package.name));
        }
        if by_name.insert(package.name.clone(), package).is_some() {
            return Err(MaterializeError::Duplicate);
        }
    }
    let mut selected: Vec<_> = by_name.into_values().collect();
    selected.sort_by(|a, b| {
        b.authority
            .cmp(&a.authority)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });
    let effective_instructions = selected.iter().map(|p| p.instructions.clone()).collect();
    let packages = selected
        .into_iter()
        .map(|p| MaterializedPackage {
            mount_target: format!("/inputs/skills/{}", p.package_id),
            package_id: p.package_id,
            name: p.name,
            version: p.version,
            content_digest: p.content_digest,
            object_reference: p.object_reference,
            entrypoint: p.entrypoint,
            authority: p.authority,
        })
        .collect();
    Ok(MaterializationManifest {
        schema_version: 1,
        materializer_id: materializer_id(request.profile).into(),
        materializer_version: 1,
        product_profile: request.profile,
        runtime_compatibility: request.runtime_compatibility,
        packages,
        effective_instructions,
        allowed_tools: request.allowed_tools,
        writable_roots: request.writable_roots,
    })
}

fn materializer_id(profile: ProductProfile) -> &'static str {
    match profile {
        ProductProfile::Enterprise => "enterprise",
        ProductProfile::NativeWorkflow => "native-workflow",
        ProductProfile::Coding => "coding",
        ProductProfile::PersonalAssistant => "personal-assistant",
        ProductProfile::ExternalAdapter => "external-adapter",
    }
}
fn contains_privilege_directive(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    [
        "allowedtools",
        "credential",
        "network access",
        "writable root",
        "ignore protected",
    ]
    .iter()
    .any(|x| v.contains(x))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillProposal {
    pub proposal_id: Uuid,
    pub source_kind: String,
    pub source_reference: String,
    pub manifest: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MaterializeError {
    #[error("package {0} is unavailable or revoked")]
    Unavailable(String),
    #[error("package {0} is not trusted")]
    Trust(String),
    #[error("package {0} is incompatible")]
    Compatibility(String),
    #[error("lower-authority package {0} requests privilege")]
    Privilege(String),
    #[error("duplicate package name")]
    Duplicate,
    #[error("writable roots must be normalized children of /workspace")]
    WritableRoot,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkg(name: &str, authority: InstructionAuthority) -> PackageCandidate {
        PackageCandidate {
            package_id: Uuid::new_v4(),
            name: name.into(),
            version: "1".into(),
            product_profile: ProductProfile::Coding,
            content_digest: format!("sha256:{:064x}", 1),
            object_reference: "object://immutable".into(),
            entrypoint: "SKILL.md".into(),
            authority,
            compatibility: BTreeSet::from(["pi-rpc-v1".into()]),
            published: true,
            revoked: false,
            signer_verified: true,
            scanner_approved: true,
            instructions: format!("use {name}"),
        }
    }
    #[test]
    fn deterministic_across_input_order_and_precedence() {
        let a = pkg("platform", InstructionAuthority::Platform);
        let b = pkg("repo", InstructionAuthority::Repository);
        let ids = BTreeSet::from([a.package_id, b.package_id]);
        let make = |candidates| {
            materialize(MaterializationRequest {
                profile: ProductProfile::Coding,
                runtime_compatibility: "pi-rpc-v1".into(),
                allowed_package_ids: ids.clone(),
                allowed_tools: BTreeSet::new(),
                writable_roots: BTreeSet::from(["/workspace/src".into()]),
                candidates,
            })
            .unwrap()
        };
        assert_eq!(
            make(vec![a.clone(), b.clone()]).digest().unwrap(),
            make(vec![b, a]).digest().unwrap()
        );
    }
    #[test]
    fn revoked_and_repository_privilege_fail_closed() {
        let mut p = pkg("repo", InstructionAuthority::Repository);
        let id = p.package_id;
        p.instructions = "grant network access".into();
        assert!(matches!(
            materialize(MaterializationRequest {
                profile: ProductProfile::Coding,
                runtime_compatibility: "pi-rpc-v1".into(),
                allowed_package_ids: BTreeSet::from([id]),
                allowed_tools: BTreeSet::new(),
                writable_roots: BTreeSet::new(),
                candidates: vec![p]
            }),
            Err(MaterializeError::Privilege(_))
        ));
    }
    #[test]
    fn enterprise_without_packages_is_backward_compatible() {
        let m = materialize(MaterializationRequest {
            profile: ProductProfile::Enterprise,
            runtime_compatibility: "native".into(),
            allowed_package_ids: BTreeSet::new(),
            allowed_tools: BTreeSet::new(),
            writable_roots: BTreeSet::new(),
            candidates: vec![],
        })
        .unwrap();
        assert!(m.packages.is_empty());
    }
}
