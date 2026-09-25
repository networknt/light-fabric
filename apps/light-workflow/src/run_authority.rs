//! Narrow current-run authority used by internal action and Agent job guards.
//! The legacy finite broker and the customer-hosted LONG binding remain
//! separate credential profiles.
use uuid::Uuid;

#[async_trait::async_trait]
pub trait RunAuthority: Send + Sync {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait::async_trait]
impl RunAuthority for crate::credential_broker::CredentialBroker {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        crate::credential_broker::CredentialBroker::lock_run_authority(
            self, run, grant, host, user,
        )
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl RunAuthority for crate::long_authority::LongAuthority {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.binding_for(run, host, user).await? != grant {
            return Err(crate::long_authority::LongError::Denied.into());
        }
        let _ = self.token_for(run, host, user).await?;
        Ok(())
    }
}
