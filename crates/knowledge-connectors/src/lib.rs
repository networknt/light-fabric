//! Provider normalization contracts for Phase 2 enterprise Knowledge sources.
//!
//! Provider cursors remain opaque and are committed only after every object and
//! permission record in a validated page has been durably applied by the worker.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use knowledge_core::{AclEffect, AclMode, AclSubject, AclSubjectType, NormalizedAclRevision};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

pub const MAX_ACL_AGE: Duration = Duration::minutes(15);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectorError {
    #[error("KNOWLEDGE_CONNECTOR_PAGE_INVALID: {0}")]
    InvalidPage(&'static str),
    #[error("KNOWLEDGE_CONNECTOR_PERMISSION_INCOMPLETE: {0}")]
    IncompletePermission(&'static str),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConnectorKind {
    SharePoint,
    Confluence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConnectorSyncMode {
    Full,
    Delta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectorObject {
    pub external_id: String,
    pub parent_external_id: Option<String>,
    pub canonical_uri: String,
    pub provider_version: String,
    pub title: String,
    pub markdown: String,
    #[serde(default)]
    pub deleted: bool,
    pub permission: ProviderPermission,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectorPage {
    pub provider: ConnectorKind,
    pub sync_mode: ConnectorSyncMode,
    pub requested_cursor: Option<String>,
    pub next_cursor: String,
    pub reconciliation_complete: bool,
    pub observed_at: DateTime<Utc>,
    pub objects: Vec<ConnectorObject>,
}

impl ConnectorPage {
    pub fn validate(self, approved_origin: &str) -> Result<ValidatedConnectorPage, ConnectorError> {
        let approved = Url::parse(approved_origin)
            .map_err(|_| ConnectorError::InvalidPage("approved origin"))?;
        let next = Url::parse(&self.next_cursor)
            .map_err(|_| ConnectorError::InvalidPage("opaque cursor"))?;
        if next.scheme() != "https"
            || next.host_str() != approved.host_str()
            || next.port_or_known_default() != approved.port_or_known_default()
            || !next.username().is_empty()
            || next.password().is_some()
            || next.fragment().is_some()
        {
            return Err(ConnectorError::InvalidPage("cursor origin"));
        }
        let mut identities = BTreeSet::new();
        for object in &self.objects {
            if object.external_id.trim().is_empty()
                || object.provider_version.trim().is_empty()
                || !identities.insert(object.external_id.as_str())
            {
                return Err(ConnectorError::InvalidPage("object identity"));
            }
            let citation = Url::parse(&object.canonical_uri)
                .map_err(|_| ConnectorError::InvalidPage("canonical URI"))?;
            if citation.scheme() != "https" {
                return Err(ConnectorError::InvalidPage("canonical URI scheme"));
            }
            if !matches!(
                (self.provider, &object.permission),
                (ConnectorKind::SharePoint, ProviderPermission::SharePoint(_))
                    | (ConnectorKind::Confluence, ProviderPermission::Confluence(_))
            ) {
                return Err(ConnectorError::InvalidPage("provider permission kind"));
            }
        }
        Ok(ValidatedConnectorPage(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedConnectorPage(ConnectorPage);

impl ValidatedConnectorPage {
    pub fn page(&self) -> &ConnectorPage {
        &self.0
    }

    pub fn committed_cursor(&self) -> &str {
        &self.0.next_cursor
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "provider",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum ProviderPermission {
    SharePoint(SharePointPermission),
    Confluence(ConfluencePermission),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderSubject {
    pub provider_id: String,
    pub subject_type: AclSubjectType,
    pub mapped_subject_id: Option<String>,
    #[serde(default = "allow_effect")]
    pub effect: AclEffect,
    pub evidence_digest: String,
}

fn allow_effect() -> AclEffect {
    AclEffect::Allow
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharePointLink {
    pub scope: String,
    #[serde(default)]
    pub recipients: Vec<ProviderSubject>,
    pub organization_id: Option<String>,
    #[serde(default)]
    pub expired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharePointPermission {
    pub inheritance_complete: bool,
    pub permission_scan_complete: bool,
    #[serde(default)]
    pub direct_subjects: Vec<ProviderSubject>,
    #[serde(default)]
    pub links: Vec<SharePointLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfluencePermission {
    pub product_access_complete: bool,
    pub space_permission_complete: bool,
    pub inherited_restrictions_complete: bool,
    pub page_restrictions_complete: bool,
    #[serde(default)]
    pub unsupported_precedence: bool,
    #[serde(default)]
    pub effective_subjects: Vec<ProviderSubject>,
}

pub fn normalize_permission(
    permission: &ProviderPermission,
    observed_at: DateTime<Utc>,
) -> NormalizedAclRevision {
    // Provider clocks are evidence, not an authority for extending the
    // authorization window. A postdated page is clamped to server time.
    let observed_at = observed_at.min(Utc::now());
    let (complete, mut subjects) = match permission {
        ProviderPermission::SharePoint(permission) => {
            let mut subjects = permission.direct_subjects.clone();
            let mut complete =
                permission.inheritance_complete && permission.permission_scan_complete;
            for link in &permission.links {
                if link.expired || link.scope == "anonymous" {
                    continue;
                }
                match link.scope.as_str() {
                    "organization" => match &link.organization_id {
                        Some(organization_id) => subjects.push(ProviderSubject {
                            provider_id: format!("organization-link:{organization_id}"),
                            subject_type: AclSubjectType::Organization,
                            mapped_subject_id: Some(organization_id.clone()),
                            effect: AclEffect::Allow,
                            evidence_digest: digest_json(link),
                        }),
                        None => complete = false,
                    },
                    "users" if !link.recipients.is_empty() => {
                        subjects.extend(link.recipients.clone());
                    }
                    _ => complete = false,
                }
            }
            (complete, subjects)
        }
        ProviderPermission::Confluence(permission) => (
            permission.product_access_complete
                && permission.space_permission_complete
                && permission.inherited_restrictions_complete
                && permission.page_restrictions_complete
                && !permission.unsupported_precedence,
            permission.effective_subjects.clone(),
        ),
    };
    subjects.sort_by(|left, right| {
        format!("{:?}:{}", left.subject_type, left.provider_id)
            .cmp(&format!("{:?}:{}", right.subject_type, right.provider_id))
    });
    let normalized = subjects
        .into_iter()
        .map(|subject| AclSubject {
            provider_subject_id: subject.provider_id,
            subject_type: subject.subject_type,
            subject_id: subject.mapped_subject_id.clone().unwrap_or_default(),
            effect: subject.effect,
            mapping_complete: subject.mapped_subject_id.is_some()
                && subject.subject_type != AclSubjectType::Unresolved,
            provider_evidence_digest: subject.evidence_digest,
        })
        .collect::<Vec<_>>();
    NormalizedAclRevision {
        mode: AclMode::MirrorSourceAcl,
        complete,
        observed_at,
        fresh_until: observed_at + MAX_ACL_AGE,
        provider_effective_decision: complete,
        subjects: normalized,
    }
}

pub fn permission_digest(acl: &NormalizedAclRevision) -> String {
    // Permission identity is semantic. Observation/freshness timestamps are
    // reconciliation evidence and must not create a new ACL revision.
    digest_json(&(
        acl.mode,
        acl.complete,
        acl.provider_effective_decision,
        &acl.subjects,
    ))
}

pub fn stable_objects(page: &ValidatedConnectorPage) -> BTreeMap<String, ConnectorObject> {
    page.page()
        .objects
        .iter()
        .cloned()
        .map(|object| (object.external_id.clone(), object))
        .collect()
}

fn digest_json(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowledge_core::{PrincipalContext, authorize_document_acl};

    fn subject(
        subject_type: AclSubjectType,
        provider: &str,
        mapped: Option<&str>,
    ) -> ProviderSubject {
        ProviderSubject {
            provider_id: provider.into(),
            subject_type,
            mapped_subject_id: mapped.map(str::to_string),
            effect: AclEffect::Allow,
            evidence_digest: "a".repeat(64),
        }
    }

    #[test]
    fn sharepoint_link_scopes_are_explicit_and_fail_closed() {
        let now = Utc::now();
        let permission = ProviderPermission::SharePoint(SharePointPermission {
            inheritance_complete: true,
            permission_scan_complete: true,
            direct_subjects: vec![subject(AclSubjectType::Group, "g-provider", Some("g-1"))],
            links: vec![
                SharePointLink {
                    scope: "anonymous".into(),
                    recipients: vec![],
                    organization_id: None,
                    expired: false,
                },
                SharePointLink {
                    scope: "organization".into(),
                    recipients: vec![],
                    organization_id: Some("org-1".into()),
                    expired: false,
                },
            ],
        });
        let acl = normalize_permission(&permission, now);
        let principal = PrincipalContext {
            subject_id: "u-1".into(),
            subject_type: "user".into(),
            groups: BTreeSet::new(),
            organizations: BTreeSet::from(["org-1".into()]),
        };
        assert!(authorize_document_acl(&acl, &principal, now));
        let unresolved = ProviderPermission::SharePoint(SharePointPermission {
            inheritance_complete: true,
            permission_scan_complete: true,
            direct_subjects: vec![],
            links: vec![SharePointLink {
                scope: "unknown".into(),
                recipients: vec![],
                organization_id: None,
                expired: false,
            }],
        });
        assert!(!normalize_permission(&unresolved, now).complete);
    }

    #[test]
    fn confluence_requires_every_effective_access_layer() {
        let now = Utc::now();
        let mut permission = ConfluencePermission {
            product_access_complete: true,
            space_permission_complete: true,
            inherited_restrictions_complete: true,
            page_restrictions_complete: true,
            unsupported_precedence: false,
            effective_subjects: vec![subject(AclSubjectType::User, "u-provider", Some("u-1"))],
        };
        assert!(
            normalize_permission(&ProviderPermission::Confluence(permission.clone()), now).complete
        );
        permission.space_permission_complete = false;
        assert!(!normalize_permission(&ProviderPermission::Confluence(permission), now).complete);
    }

    #[test]
    fn cursor_is_same_origin_and_not_released_before_validation() {
        let page = ConnectorPage {
            provider: ConnectorKind::SharePoint,
            sync_mode: ConnectorSyncMode::Delta,
            requested_cursor: None,
            next_cursor: "https://graph.microsoft.com/v1.0/drives/delta?token=opaque".into(),
            reconciliation_complete: true,
            observed_at: Utc::now(),
            objects: vec![],
        };
        assert!(page.clone().validate("https://graph.microsoft.com").is_ok());
        assert_eq!(
            page.validate("https://evil.invalid"),
            Err(ConnectorError::InvalidPage("cursor origin"))
        );
    }

    #[test]
    fn provider_time_cannot_extend_acl_freshness_past_server_ceiling() {
        let server_before = Utc::now();
        let acl = normalize_permission(
            &ProviderPermission::Confluence(ConfluencePermission {
                product_access_complete: true,
                space_permission_complete: true,
                inherited_restrictions_complete: true,
                page_restrictions_complete: true,
                unsupported_precedence: false,
                effective_subjects: Vec::new(),
            }),
            server_before + chrono::Duration::days(1),
        );
        assert!(acl.observed_at >= server_before);
        assert!(acl.observed_at <= Utc::now());
        assert_eq!(acl.fresh_until, acl.observed_at + MAX_ACL_AGE);
    }

    #[test]
    fn permission_digest_ignores_reconciliation_timestamps() {
        let permission = ProviderPermission::Confluence(ConfluencePermission {
            product_access_complete: true,
            space_permission_complete: true,
            inherited_restrictions_complete: true,
            page_restrictions_complete: true,
            unsupported_precedence: false,
            effective_subjects: Vec::new(),
        });
        let first = normalize_permission(&permission, Utc::now() - chrono::Duration::minutes(2));
        let second = normalize_permission(&permission, Utc::now());
        assert_eq!(permission_digest(&first), permission_digest(&second));
    }
}
