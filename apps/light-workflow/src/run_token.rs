//! Selects the user bearer for a live run at the outbound boundary.
use crate::{long_authority::LongAuthority, run_credential::RunCredentialVault};
use chrono::{DateTime, Utc};
use light_client::long_binding::LongBindingStore;
use light_security::{
    JwtExpiryMode, SecurityRuntime,
    token_purpose::{TokenUse, validate_verified_purpose},
    verify_jwt_token,
};
use sqlx::{PgPool, Row};
use std::{io, sync::Arc};
use uuid::Uuid;

type TokenError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTokenSource {
    Original,
    LongExchange,
}

pub struct SelectedRunToken {
    pub token: String,
    pub source: RunTokenSource,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "run user credential denied",
    )
}

#[derive(Clone)]
pub struct RunTokenSelector {
    pool: PgPool,
    vault: Option<Arc<RunCredentialVault>>,
    long: Option<Arc<LongAuthority>>,
    security: Arc<SecurityRuntime>,
    margin_seconds: i64,
}

impl RunTokenSelector {
    pub(crate) fn security_runtime(&self) -> &SecurityRuntime {
        &self.security
    }
    pub async fn run_for_process(
        &self,
        host: Uuid,
        process: Uuid,
    ) -> Result<Option<(Uuid, Uuid, String)>, TokenError> {
        let row = sqlx::query(
            "SELECT i.workflow_instance_id,a.user_id,a.credential_kind
            FROM workflow_ops.workflow_invocation_t i
            JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id
            JOIN workflow_ops.workflow_action_authority_t a
              ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id
            WHERE i.host_id=$1 AND i.process_id=$2 AND a.active AND a.deadline>clock_timestamp()
              AND a.user_id::text=i.end_user_subject
              AND i.state IN ('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL
              AND (i.deadline_ts>clock_timestamp()
                   OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1')
              AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp())",
        )
        .bind(host)
        .bind(process)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| {
            (
                row.get("workflow_instance_id"),
                row.get("user_id"),
                row.get("credential_kind"),
            )
        }))
    }

    pub fn new(
        pool: PgPool,
        vault: Option<Arc<RunCredentialVault>>,
        long: Option<Arc<LongAuthority>>,
        security: Arc<SecurityRuntime>,
        margin_seconds: i64,
    ) -> Result<Self, TokenError> {
        if !(0..=86_400).contains(&margin_seconds) {
            return Err(denied().into());
        }
        Ok(Self {
            pool,
            vault,
            long,
            security,
            margin_seconds,
        })
    }

    pub async fn select_run_token(
        &self,
        run: Uuid,
        host: Uuid,
        user: Uuid,
        now: DateTime<Utc>,
    ) -> Result<String, TokenError> {
        Ok(self
            .select_run_token_with_source(run, host, user, now)
            .await?
            .token)
    }

    pub async fn select_run_token_with_source(
        &self,
        run: Uuid,
        host: Uuid,
        user: Uuid,
        now: DateTime<Utc>,
    ) -> Result<SelectedRunToken, TokenError> {
        let row = sqlx::query(
            "SELECT a.credential_kind,a.grant_id
             FROM workflow_ops.workflow_action_authority_t a
             JOIN workflow_ops.workflow_invocation_t i
               ON i.host_id=a.host_id AND i.workflow_instance_id=a.run_id
             JOIN workflow_ops.process_info_t p
               ON p.host_id=i.host_id AND p.process_id=i.process_id
             WHERE a.host_id=$1 AND a.run_id=$2 AND a.user_id=$3
               AND a.active AND a.deadline>$4 AND a.user_id::text=i.end_user_subject
               AND i.state IN ('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL
               AND (i.deadline_ts>$4
                    OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1')
               AND (p.deadline_ts IS NULL OR p.deadline_ts>$4)",
        )
        .bind(host)
        .bind(run)
        .bind(user)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(denied)?;
        match row.get::<&str, _>("credential_kind") {
            "invoke" => {
                let vault = self.vault.as_ref().ok_or_else(denied)?;
                let credential = sqlx::query("SELECT c.key_id,c.token_bytes,c.token_exp
                    FROM workflow_ops.workflow_run_credential_t c
                    JOIN workflow_ops.workflow_invocation_t i ON i.host_id=c.host_id AND i.workflow_instance_id=c.workflow_instance_id
                    WHERE c.host_id=$1 AND c.workflow_instance_id=$2 AND c.expires_ts>$3
                      AND i.state IN ('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL
                      AND i.deadline_ts>$3 AND i.end_user_subject=$4")
                    .bind(host).bind(run).bind(now).bind(user.to_string())
                    .fetch_optional(&self.pool).await?.ok_or_else(denied)?;
                let original = vault.open(
                    credential.get("key_id"),
                    run,
                    credential.get::<Vec<u8>, _>("token_bytes").as_slice(),
                )?;
                let exp = self.verify_original(&original, host, user).await?;
                if exp != credential.get::<i64, _>("token_exp")
                    || select_token_decision(
                        "invoke",
                        true,
                        exp,
                        now.timestamp(),
                        self.margin_seconds,
                    ) != TokenDecision::Original
                {
                    return Err(denied().into());
                }
                Ok(SelectedRunToken {
                    token: original,
                    source: RunTokenSource::Original,
                })
            }
            "long" => {
                let long = self.long.as_ref().ok_or_else(denied)?;
                let original = LongBindingStore::active(long.as_ref(), run, host, user)
                    .await?
                    .ok_or_else(denied)?;
                if original.binding_id != row.get::<Uuid, _>("grant_id") {
                    return Err(denied().into());
                }
                let exp = self
                    .verify_original(&original.source_token, host, user)
                    .await?;
                match select_token_decision("long", true, exp, now.timestamp(), self.margin_seconds)
                {
                    TokenDecision::Original => Ok(SelectedRunToken {
                        token: (*original.source_token).clone(),
                        source: RunTokenSource::Original,
                    }),
                    TokenDecision::Exchange => Ok(SelectedRunToken {
                        token: long.token_for(run, host, user).await?,
                        source: RunTokenSource::LongExchange,
                    }),
                    TokenDecision::Denied => Err(denied().into()),
                }
            }
            // Historic broker rows have no configured source after retirement.
            _ => Err(denied().into()),
        }
    }

    async fn verify_original(
        &self,
        token: &str,
        host: Uuid,
        user: Uuid,
    ) -> Result<i64, TokenError> {
        let principal = verify_jwt_token(&self.security, token, JwtExpiryMode::Ignore)
            .await
            .map_err(|_| denied())?;
        validate_verified_purpose(token, &principal, TokenUse::User, &[]).map_err(|_| denied())?;
        let token_host = principal
            .host
            .as_deref()
            .or_else(|| {
                principal
                    .claims
                    .get("hostId")
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                principal
                    .claims
                    .get("host_id")
                    .and_then(serde_json::Value::as_str)
            });
        if token_host != Some(host.to_string().as_str()) {
            return Err(denied().into());
        }
        let token_user = principal
            .user_id
            .as_deref()
            .or_else(|| {
                principal
                    .claims
                    .get("user_id")
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                principal
                    .claims
                    .get("userId")
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                principal
                    .claims
                    .get("sub")
                    .and_then(serde_json::Value::as_str)
            });
        if token_user != Some(user.to_string().as_str()) {
            return Err(denied().into());
        }
        principal
            .claims
            .get("exp")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| denied().into())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TokenDecision {
    Original,
    Exchange,
    Denied,
}

fn select_token_decision(
    kind: &str,
    verified: bool,
    exp: i64,
    now: i64,
    margin_seconds: i64,
) -> TokenDecision {
    if !verified {
        return TokenDecision::Denied;
    }
    match kind {
        "long" if exp.saturating_sub(now) > margin_seconds => TokenDecision::Original,
        "long" => TokenDecision::Exchange,
        "invoke" if exp > now => TokenDecision::Original,
        _ => TokenDecision::Denied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_controls_every_token_decision_branch() {
        let now = 1_000;
        assert_eq!(
            select_token_decision("long", true, 1_061, now, 60),
            TokenDecision::Original
        );
        assert_eq!(
            select_token_decision("long", true, 1_060, now, 60),
            TokenDecision::Exchange
        );
        assert_eq!(
            select_token_decision("long", true, 1_000, now, 60),
            TokenDecision::Exchange
        );
        assert_eq!(
            select_token_decision("invoke", true, 1_001, now, 60),
            TokenDecision::Original
        );
        assert_eq!(
            select_token_decision("invoke", true, 1_000, now, 60),
            TokenDecision::Denied
        );
        assert_eq!(
            select_token_decision("invoke", false, 1_500, now, 60),
            TokenDecision::Denied
        );
        assert_eq!(
            select_token_decision("long", false, 1_500, now, 60),
            TokenDecision::Denied
        );
        assert_eq!(
            select_token_decision("broker", true, 1_500, now, 60),
            TokenDecision::Denied
        );
    }
}
