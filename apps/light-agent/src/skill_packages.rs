use agent_core::sha256_digest;
use agent_materializer::{InstructionAuthority, ProductProfile};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

#[async_trait]
pub trait SkillPackageStore: Send + Sync {
    async fn put_immutable(&self, key: &str, bytes: &[u8]) -> Result<String, PackageStoreError>;
    async fn delete(&self, reference: &str) -> Result<(), PackageStoreError>;
    async fn exists(&self, reference: &str) -> Result<bool, PackageStoreError>;
}
#[derive(Debug, Error)]
#[error("skill package store failed: {0}")]
pub struct PackageStoreError(pub String);

pub struct PublishSkillPackage<'a> {
    pub host_id: Uuid,
    pub package_id: Uuid,
    pub name: &'a str,
    pub version: &'a str,
    pub profile: ProductProfile,
    pub media_type: &'a str,
    pub bytes: &'a [u8],
    pub signer_reference: &'a str,
    pub signature_reference: &'a str,
    pub scanner_reference: &'a str,
    pub scan_digest: &'a str,
    pub provenance_reference: &'a str,
    pub entrypoint: &'a str,
    pub compatibility: Value,
    pub authority: InstructionAuthority,
    pub reviewed_by: &'a str,
    pub retain_until: DateTime<Utc>,
}

pub async fn publish<S: SkillPackageStore>(
    pool: &PgPool,
    store: &S,
    p: PublishSkillPackage<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    if p.signature_reference.is_empty()
        || p.provenance_reference.is_empty()
        || p.reviewed_by.is_empty()
        || p.retain_until <= Utc::now()
    {
        return Err("package trust, review, and retention evidence are required".into());
    }
    let digest = sha256_digest(p.bytes);
    let reference = store
        .put_immutable(&format!("skills/{}/{}", p.package_id, digest), p.bytes)
        .await?;
    sqlx::query("INSERT INTO skill_package_t(host_id,package_id,package_name,package_version,product_profile,object_reference,content_digest,media_type,size_bytes,signer_reference,signature_reference,scanner_reference,scan_digest,provenance_reference,entrypoint,compatibility,instruction_authority,state,reviewed_by,reviewed_ts,retain_until_ts) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,'PUBLISHED',$18,now(),$19)")
      .bind(p.host_id).bind(p.package_id).bind(p.name).bind(p.version).bind(profile_name(p.profile)).bind(reference).bind(&digest).bind(p.media_type).bind(p.bytes.len() as i64).bind(p.signer_reference).bind(p.signature_reference).bind(p.scanner_reference).bind(p.scan_digest).bind(p.provenance_reference).bind(p.entrypoint).bind(p.compatibility).bind(authority_name(p.authority)).bind(p.reviewed_by).bind(p.retain_until).execute(pool).await?;
    Ok(digest)
}

pub async fn revoke(
    pool: &PgPool,
    host_id: Uuid,
    package_id: Uuid,
    actor: &str,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE skill_package_t SET state='REVOKED',revoked_ts=now(),revocation_reason=$4,deletion_state=CASE WHEN retain_until_ts<=now() THEN 'DELETE_PENDING' ELSE deletion_state END,deletion_evidence=jsonb_build_object('revokedBy',$3),updated_ts=now() WHERE host_id=$1 AND package_id=$2 AND state='PUBLISHED'").bind(host_id).bind(package_id).bind(actor).bind(reason).execute(pool).await?;
    Ok(())
}

pub async fn reconcile_retention<S: SkillPackageStore>(
    pool: &PgPool,
    store: &S,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let rows=sqlx::query("UPDATE skill_package_t SET deletion_state='DELETING',updated_ts=now() WHERE (host_id,package_id) IN (SELECT host_id,package_id FROM skill_package_t WHERE state<>'PUBLISHED' AND retain_until_ts<=now() AND deletion_state IN ('RETAINED','DELETE_PENDING','DELETE_FAILED') ORDER BY retain_until_ts FOR UPDATE SKIP LOCKED LIMIT $1) RETURNING host_id,package_id,object_reference").bind(limit.clamp(1,200)).fetch_all(pool).await?;
    for row in &rows {
        let host: Uuid = row.get("host_id");
        let id: Uuid = row.get("package_id");
        let reference: String = row.get("object_reference");
        let outcome = match store.delete(&reference).await {
            Ok(()) => async_std_absence(store, &reference).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                sqlx::query("UPDATE skill_package_t SET deletion_state='DELETED',deletion_evidence=jsonb_build_object('verifiedAbsent',true),updated_ts=now() WHERE host_id=$1 AND package_id=$2").bind(host).bind(id).execute(pool).await?;
            }
            Err(e) => {
                sqlx::query("UPDATE skill_package_t SET deletion_state='DELETE_FAILED',deletion_evidence=jsonb_build_object('error',$3),retain_until_ts=now()+interval '5 minutes',updated_ts=now() WHERE host_id=$1 AND package_id=$2").bind(host).bind(id).bind(e.to_string()).execute(pool).await?;
            }
        }
    }
    Ok(rows.len() as u64)
}
async fn async_std_absence<S: SkillPackageStore>(
    store: &S,
    r: &str,
) -> Result<(), PackageStoreError> {
    if store.exists(r).await? {
        Err(PackageStoreError("object remains after delete".into()))
    } else {
        Ok(())
    }
}
fn profile_name(v: ProductProfile) -> &'static str {
    match v {
        ProductProfile::Enterprise => "enterprise",
        ProductProfile::NativeWorkflow => "native-workflow",
        ProductProfile::Coding => "coding",
        ProductProfile::PersonalAssistant => "personal-assistant",
        ProductProfile::ExternalAdapter => "external-adapter",
    }
}
fn authority_name(v: InstructionAuthority) -> &'static str {
    match v {
        InstructionAuthority::Platform => "platform",
        InstructionAuthority::Product => "product",
        InstructionAuthority::Administrator => "administrator",
        InstructionAuthority::Repository => "repository",
        InstructionAuthority::Generated => "generated",
    }
}

pub async fn diagnostics(pool: &PgPool, host_id: Uuid) -> Result<Vec<Value>, sqlx::Error> {
    let rows=sqlx::query("SELECT package_id,package_name,package_version,product_profile,content_digest,state,compatibility,reviewed_by,reviewed_ts,revoked_ts,deletion_state FROM skill_package_t WHERE host_id=$1 ORDER BY package_name,package_version").bind(host_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r|serde_json::json!({"packageId":r.get::<Uuid,_>("package_id"),"name":r.get::<String,_>("package_name"),"version":r.get::<String,_>("package_version"),"profile":r.get::<String,_>("product_profile"),"digest":r.get::<String,_>("content_digest"),"state":r.get::<String,_>("state"),"compatibility":r.get::<Value,_>("compatibility"),"reviewedBy":r.get::<String,_>("reviewed_by"),"reviewedAt":r.get::<DateTime<Utc>,_>("reviewed_ts"),"revokedAt":r.get::<Option<DateTime<Utc>>,_>("revoked_ts"),"deletionState":r.get::<String,_>("deletion_state")})).collect())
}
