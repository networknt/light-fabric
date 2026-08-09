use std::collections::BTreeSet;

use chrono::{TimeZone, Utc};
use knowledge_connectors::{
    ConfluencePermission, ProviderPermission, ProviderSubject, SharePointLink,
    SharePointPermission, normalize_permission,
};
use knowledge_core::{AclEffect, AclSubjectType, PrincipalContext, authorize_document_acl};

fn subject(
    kind: AclSubjectType,
    provider: &str,
    mapped: Option<&str>,
    effect: AclEffect,
) -> ProviderSubject {
    ProviderSubject {
        provider_id: provider.into(),
        subject_type: kind,
        mapped_subject_id: mapped.map(str::to_string),
        effect,
        evidence_digest: "a".repeat(64),
    }
}

fn main() {
    let observed = Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap();
    let principal = PrincipalContext {
        subject_id: "user-1".into(),
        subject_type: "USER".into(),
        groups: BTreeSet::from(["engineering".into()]),
        organizations: BTreeSet::from(["tenant-1".into()]),
    };
    let sharepoint = normalize_permission(
        &ProviderPermission::SharePoint(SharePointPermission {
            inheritance_complete: true,
            permission_scan_complete: true,
            direct_subjects: vec![subject(
                AclSubjectType::Group,
                "sp-group-7",
                Some("engineering"),
                AclEffect::Allow,
            )],
            links: vec![SharePointLink {
                scope: "anonymous".into(),
                recipients: vec![],
                organization_id: None,
                expired: false,
            }],
        }),
        observed,
    );
    assert!(authorize_document_acl(
        &sharepoint,
        &principal,
        observed + chrono::Duration::minutes(1)
    ));
    assert!(!authorize_document_acl(
        &sharepoint,
        &principal,
        observed + chrono::Duration::minutes(15)
    ));

    let confluence = normalize_permission(
        &ProviderPermission::Confluence(ConfluencePermission {
            product_access_complete: true,
            space_permission_complete: true,
            inherited_restrictions_complete: false,
            page_restrictions_complete: true,
            unsupported_precedence: false,
            effective_subjects: vec![subject(
                AclSubjectType::User,
                "conf-user-1",
                Some("user-1"),
                AclEffect::Allow,
            )],
        }),
        observed,
    );
    assert!(!authorize_document_acl(&confluence, &principal, observed));
    println!("{{\"status\":\"PASS\",\"phase\":\"2\",\"providers\":2}}");
}
