//! Encrypted, per-run original user credential storage for workflow-backed Tools.
use crate::long_authority::{Keys, LongError};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use std::path::Path;
use uuid::Uuid;

pub struct RunCredentialVault {
    keys: Keys,
}

pub struct SealedRunCredential {
    key_id: String,
    token_bytes: Vec<u8>,
    token_exp: i64,
}

impl RunCredentialVault {
    pub async fn load(dir: &Path, keyring_file: Option<&Path>) -> Result<Option<Self>, LongError> {
        match keyring_file {
            Some(path) => Ok(Some(Self {
                keys: Keys::load(&dir.join(path)).await?,
            })),
            None => Ok(None),
        }
    }

    pub fn seal(
        &self,
        run_id: Uuid,
        token: &str,
        token_exp: i64,
    ) -> Result<SealedRunCredential, LongError> {
        if token_exp <= 0 {
            return Err(LongError::Evidence);
        }
        Ok(SealedRunCredential {
            key_id: self.keys.active.clone(),
            token_bytes: self.keys.seal_for("run-credential", run_id, token)?,
            token_exp,
        })
    }

    pub fn open(&self, key_id: &str, run_id: Uuid, bytes: &[u8]) -> Result<String, LongError> {
        if key_id == "plaintext" {
            return Err(LongError::Evidence);
        }
        self.keys.open_for("run-credential", key_id, run_id, bytes)
    }
}

impl SealedRunCredential {
    pub async fn insert(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        run: Uuid,
        deadline: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO workflow_run_credential_t(host_id,workflow_instance_id,key_id,token_bytes,token_exp,expires_ts) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(host).bind(run).bind(&self.key_id).bind(&self.token_bytes).bind(self.token_exp).bind(deadline)
            .execute(&mut **tx).await?;
        Ok(())
    }

    pub async fn refresh_if_later(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        run: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_run_credential_t SET key_id=$3,token_bytes=$4,token_exp=$5,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND workflow_instance_id=$2 AND token_exp < $5 AND EXISTS (SELECT 1 FROM workflow_invocation_t i WHERE i.host_id=$1 AND i.workflow_instance_id=$2 AND i.state NOT IN ('COMPLETED','FAILED','CANCELLED'))")
            .bind(host).bind(run).bind(&self.key_id).bind(&self.token_bytes).bind(self.token_exp)
            .execute(&mut **tx).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn absent_keyring_refuses_vault() {
        assert!(
            RunCredentialVault::load(Path::new("."), None)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn sealed_credential_round_trips_and_refuses_plaintext() {
        let path =
            std::env::temp_dir().join(format!("workflow-vault-test-{}.json", Uuid::new_v4()));
        tokio::fs::write(&path, r#"{"activeKeyId":"test","keys":{"test":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#)
            .await.unwrap();
        let vault = RunCredentialVault::load(Path::new("/"), Some(&path))
            .await
            .unwrap()
            .unwrap();
        let run = Uuid::new_v4();
        let sealed = vault.seal(run, "test bearer", 100).unwrap();
        assert_ne!(sealed.token_bytes, b"test bearer");
        assert_eq!(
            vault
                .open(&sealed.key_id, run, &sealed.token_bytes)
                .unwrap(),
            "test bearer"
        );
        assert!(vault.open("plaintext", run, b"test bearer").is_err());
        assert!(
            vault
                .open(&sealed.key_id, Uuid::new_v4(), &sealed.token_bytes)
                .is_err()
        );
        tokio::fs::remove_file(path).await.unwrap();
    }
}
