use agent_core::{AgentSessionId, PolicySnapshot, sha256_digest};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{Duration, Timelike, Utc};
use hmac::{Hmac, Mac};
use light_agent::domain::{AgentRepository, SessionSpec};
use light_agent_channel::{
    ChannelBinding,
    credential::ConnectorCredentialStore,
    slack::{self, SlackInbound},
};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration as StdDuration};
use uuid::Uuid;

const SLACK_CONNECTOR_ALIAS: &str = "slack-api-v1";
const SLACK_POST_MESSAGE_OPERATION: &str = "chat.postMessage";
const SLACK_DOWNLOAD_OPERATION: &str = "files.download";

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    repository: AgentRepository,
    host_id: Uuid,
    signing_secret: Arc<Vec<u8>>,
    connector_credentials: Arc<ConnectorCredentialStore>,
    http: reqwest::Client,
    attachment_scanner_url: Option<reqwest::Url>,
    attachment_scanner_token: Option<Arc<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&env::var("DATABASE_URL")?)
        .await?;
    let host_id = Uuid::parse_str(&env::var("LIGHT_AGENT_CHANNEL_HOST_ID")?)?;
    let secret = env::var("SLACK_SIGNING_SECRET")?.into_bytes();
    if secret.len() < 32 {
        anyhow::bail!("SLACK_SIGNING_SECRET must contain at least 32 bytes");
    }
    let attachment_scanner_url = env::var("LIGHT_AGENT_ATTACHMENT_SCANNER_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| reqwest::Url::parse(&v))
        .transpose()?;
    let attachment_scanner_token = env::var("LIGHT_AGENT_ATTACHMENT_SCANNER_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(Arc::new);
    if attachment_scanner_url
        .as_ref()
        .is_some_and(|url| url.scheme() != "https")
        || attachment_scanner_url.is_some() != attachment_scanner_token.is_some()
    {
        anyhow::bail!("attachment scanner requires an HTTPS URL and token together");
    }
    let connector_credentials = ConnectorCredentialStore::load(PathBuf::from(env::var(
        "LIGHT_AGENT_CONNECTOR_CREDENTIALS_FILE",
    )?))
    .map_err(anyhow::Error::msg)?;
    let state = AppState {
        repository: AgentRepository::new(pool.clone()),
        pool,
        host_id,
        signing_secret: Arc::new(secret),
        connector_credentials: Arc::new(connector_credentials),
        http: reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        attachment_scanner_url,
        attachment_scanner_token,
    };
    tokio::spawn(delivery_loop(state.clone()));
    tokio::spawn(trigger_loop(state.clone()));
    tokio::spawn(attachment_recovery_loop(state.clone()));
    let app = Router::new()
        .route("/channels/slack/events", post(slack_events))
        .route("/channels/connectors/events", post(connector_events))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);
    let addr: SocketAddr = env::var("LIGHT_AGENT_CHANNEL_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8440".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn connector_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match handle_connector(&state, &headers, &body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::warn!(%error,"rejected connector event");
            (StatusCode::UNAUTHORIZED, "invalid request").into_response()
        }
    }
}

async fn handle_connector(state: &AppState, headers: &HeaderMap, raw: &[u8]) -> Result<()> {
    // This parse supplies only a lookup selector. No envelope field becomes
    // authoritative until the exact raw bytes pass the selected grant's MAC.
    let selector: Value = serde_json::from_slice(raw)?;
    let selected_trigger_id = Uuid::parse_str(
        selector
            .get("triggerId")
            .and_then(Value::as_str)
            .context("triggerId missing")?,
    )?;
    let selected = sqlx::query(
        "SELECT g.connector_alias,g.credential_reference
         FROM agent_trigger_t t
         JOIN agent_connector_grant_t g ON g.host_id=t.host_id AND g.grant_id=t.connector_grant_id
         WHERE t.host_id=$1 AND t.trigger_id=$2 AND t.state='ACTIVE' AND t.trigger_kind='CONNECTOR'
           AND g.revoked_ts IS NULL AND g.expires_ts>now() AND g.use_count<g.maximum_uses
           AND g.allowed_operations ? 'events.receive'",
    )
    .bind(state.host_id)
    .bind(selected_trigger_id)
    .fetch_one(&state.pool)
    .await?;
    let connector_alias: String = selected.try_get("connector_alias")?;
    let credential_reference: String = selected.try_get("credential_reference")?;
    let secret = state
        .connector_credentials
        .secret(&credential_reference, &connector_alias)
        .map_err(anyhow::Error::msg)?;
    if secret.len() < 32 {
        anyhow::bail!("connector grant signing secret is too short");
    }
    let signature = headers
        .get("x-light-signature")
        .and_then(|v| v.to_str().ok())
        .context("missing connector signature")?
        .strip_prefix("sha256=")
        .context("invalid connector signature scheme")?;
    let supplied = hex::decode(signature)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
    mac.update(raw);
    mac.verify_slice(&supplied)?;
    let body: Value = serde_json::from_slice(raw)?;
    let trigger_id = Uuid::parse_str(
        body.get("triggerId")
            .and_then(Value::as_str)
            .context("triggerId missing")?,
    )?;
    if trigger_id != selected_trigger_id {
        anyhow::bail!("authenticated connector trigger differs from selector");
    }
    let event_id = body
        .get("eventId")
        .and_then(Value::as_str)
        .context("eventId missing")?;
    let timestamp = body
        .get("timestamp")
        .and_then(Value::as_str)
        .context("timestamp missing")?
        .parse::<chrono::DateTime<Utc>>()?;
    if (Utc::now() - timestamp).num_seconds().abs() > 300 {
        anyhow::bail!("connector event is stale");
    }
    let row=sqlx::query("SELECT t.binding_id,b.principal_id,b.agent_def_id,b.adapter_id,b.external_identity,b.allowed_destinations,b.group_allowed,b.maximum_attachment_bytes,b.quiet_hours,b.revoked_ts,g.grant_id
      FROM agent_trigger_t t JOIN agent_channel_binding_t b ON b.host_id=t.host_id AND b.binding_id=t.binding_id
      JOIN agent_connector_grant_t g ON g.host_id=t.host_id AND g.grant_id=t.connector_grant_id
      WHERE t.host_id=$1 AND t.trigger_id=$2 AND t.state='ACTIVE' AND t.trigger_kind='CONNECTOR' AND b.revoked_ts IS NULL AND g.revoked_ts IS NULL AND g.expires_ts>now() AND g.use_count<g.maximum_uses AND g.allowed_operations ? 'events.receive'")
      .bind(state.host_id).bind(trigger_id).fetch_one(&state.pool).await?;
    let destination = body
        .get("destination")
        .and_then(Value::as_str)
        .context("destination missing")?;
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .context("text missing")?;
    let quiet: Value = row.try_get("quiet_hours")?;
    let binding = ChannelBinding {
        binding_id: row.try_get("binding_id")?,
        host_id: state.host_id,
        principal_id: row.try_get("principal_id")?,
        agent_def_id: row.try_get("agent_def_id")?,
        adapter_id: row.try_get("adapter_id")?,
        external_identity: row.try_get("external_identity")?,
        allowed_destinations: serde_json::from_value(row.try_get("allowed_destinations")?)?,
        group_allowed: row.try_get("group_allowed")?,
        maximum_attachment_bytes: row.try_get::<i64, _>("maximum_attachment_bytes")? as u64,
        quiet_start_hour: quiet.get("startHour").and_then(Value::as_u64).unwrap_or(22) as u8,
        quiet_end_hour: quiet.get("endHour").and_then(Value::as_u64).unwrap_or(7) as u8,
        revoked_at: None,
    };
    if !binding.allowed_destinations.contains(destination)
        || light_agent_channel::quiet_hours(&binding, Utc::now())
    {
        anyhow::bail!("connector destination or quiet-hours policy denied event");
    }
    let message_id = Uuid::now_v7();
    let key = format!("connector:{trigger_id}:{event_id}");
    let mut tx = state.pool.begin().await?;
    let inserted=sqlx::query("INSERT INTO agent_channel_message_t(host_id,message_id,binding_id,external_event_id,response_destination,direction,payload_digest,state,payload) VALUES($1,$2,$3,$4,$5,'INBOUND',$6,'RECEIVED',$7) ON CONFLICT(host_id,binding_id,external_event_id,direction) DO NOTHING")
      .bind(state.host_id).bind(message_id).bind(binding.binding_id).bind(&key).bind(destination).bind(sha256_digest(raw)).bind(json!({"text":text,"provider":"connector"})).execute(&mut *tx).await?;
    if inserted.rows_affected() == 1 {
        let consumed=sqlx::query("UPDATE agent_connector_grant_t SET use_count=use_count+1 WHERE host_id=$1 AND grant_id=$2 AND revoked_ts IS NULL AND expires_ts>now() AND use_count<maximum_uses AND allowed_operations ? 'events.receive'").bind(state.host_id).bind(row.try_get::<Uuid,_>("grant_id")?).execute(&mut *tx).await?;
        if consumed.rows_affected() != 1 {
            tx.rollback().await?;
            anyhow::bail!("connector grant was exhausted or revoked during admission");
        }
    }
    tx.commit().await?;
    if inserted.rows_affected() == 1 {
        admit_channel_turn(state, &binding, &key, text, message_id).await?;
    }
    Ok(())
}

async fn slack_events(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match handle_slack(&state, &headers, &body).await {
        Ok(Some(challenge)) => (StatusCode::OK, challenge).into_response(),
        Ok(None) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::warn!(%error,"rejected Slack event");
            (StatusCode::UNAUTHORIZED, "invalid request").into_response()
        }
    }
}

async fn handle_slack(state: &AppState, headers: &HeaderMap, raw: &[u8]) -> Result<Option<String>> {
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .context("missing Slack timestamp")?;
    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .context("missing Slack signature")?;
    let inbound =
        slack::verify_and_parse(&state.signing_secret, timestamp, signature, raw, Utc::now())?;
    let SlackInbound::Message(message) = inbound else {
        return Ok(match inbound {
            SlackInbound::Challenge(c) => Some(c),
            _ => None,
        });
    };
    if message.text.len() > 64 * 1024 {
        anyhow::bail!("Slack message exceeds input limit");
    }
    let row = sqlx::query(
        "SELECT binding_id,principal_id,agent_def_id,allowed_destinations,
            group_allowed,maximum_attachment_bytes,quiet_hours,revoked_ts
        FROM agent_channel_binding_t WHERE host_id=$1 AND adapter_id=$2 AND external_identity=$3",
    )
    .bind(state.host_id)
    .bind(slack::ADAPTER_ID)
    .bind(&message.external_identity)
    .fetch_optional(&state.pool)
    .await?
    .context("Slack identity is not paired")?;
    let quiet: Value = row.try_get("quiet_hours")?;
    let binding = ChannelBinding {
        binding_id: row.try_get("binding_id")?,
        host_id: state.host_id,
        principal_id: row.try_get("principal_id")?,
        agent_def_id: row.try_get("agent_def_id")?,
        adapter_id: slack::ADAPTER_ID.into(),
        external_identity: message.external_identity.clone(),
        allowed_destinations: serde_json::from_value(row.try_get("allowed_destinations")?)?,
        group_allowed: row.try_get("group_allowed")?,
        maximum_attachment_bytes: row.try_get::<i64, _>("maximum_attachment_bytes")? as u64,
        quiet_start_hour: quiet.get("startHour").and_then(Value::as_u64).unwrap_or(22) as u8,
        quiet_end_hour: quiet.get("endHour").and_then(Value::as_u64).unwrap_or(7) as u8,
        revoked_at: row.try_get("revoked_ts")?,
    };
    if binding.revoked_at.is_some()
        || !binding.allowed_destinations.contains(&message.destination)
        || (message.group && !binding.group_allowed)
    {
        anyhow::bail!("Slack destination is not authorized");
    }
    let digest = sha256_digest(raw);
    let message_id = Uuid::now_v7();
    let inserted=sqlx::query_scalar::<_,Uuid>("INSERT INTO agent_channel_message_t(host_id,message_id,binding_id,
            external_event_id,response_destination,direction,payload_digest,state,payload)
        VALUES($1,$2,$3,$4,$5,'INBOUND',$6,'RECEIVED',$7)
        ON CONFLICT(host_id,binding_id,external_event_id,direction) DO NOTHING RETURNING message_id")
        .bind(state.host_id).bind(message_id).bind(binding.binding_id).bind(&message.event_id)
        .bind(&message.destination).bind(digest).bind(json!({"text":message.text,"provider":"slack","eventId":message.event_id,"attachments":message.attachments}))
        .fetch_optional(&state.pool).await?;
    if inserted.is_none() {
        return Ok(None);
    }
    let mut turn_text = message.text.clone();
    if !message.attachments.is_empty() {
        let owned_state = state.clone();
        let owned_binding = binding.clone();
        let event_id = message.event_id.clone();
        let attachments = message.attachments.clone();
        tokio::spawn(async move {
            let result = async {
                let references =
                    scan_slack_attachments(&owned_state, &owned_binding, message_id, &attachments)
                        .await?;
                turn_text.push_str("\n\nApproved scanned attachments:\n");
                for reference in references {
                    turn_text.push_str(&format!("- {reference}\n"));
                }
                admit_channel_turn(
                    &owned_state,
                    &owned_binding,
                    &event_id,
                    &turn_text,
                    message_id,
                )
                .await
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%error,message_id=%message_id,"attachment processing rejected inbound message");
                let _=sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','attachment_rejected'),updated_ts=now() WHERE host_id=$1 AND message_id=$2 AND state='RECEIVED'").bind(owned_state.host_id).bind(message_id).execute(&owned_state.pool).await;
            }
        });
        return Ok(None);
    }
    admit_channel_turn(state, &binding, &message.event_id, &turn_text, message_id).await?;
    Ok(None)
}

async fn scan_slack_attachments(
    state: &AppState,
    binding: &ChannelBinding,
    message_id: Uuid,
    attachments: &[slack::SlackAttachment],
) -> Result<Vec<String>> {
    let scanner = state
        .attachment_scanner_url
        .as_ref()
        .context("attachments require LIGHT_AGENT_ATTACHMENT_SCANNER_URL")?;
    let scanner_token = state
        .attachment_scanner_token
        .as_ref()
        .context("attachments require LIGHT_AGENT_ATTACHMENT_SCANNER_TOKEN")?;
    let mut total = 0_u64;
    let mut references = Vec::new();
    for attachment in attachments {
        total = total
            .checked_add(attachment.size_bytes)
            .context("attachment size overflow")?;
        if total > binding.maximum_attachment_bytes {
            anyhow::bail!("attachment limit exceeded");
        }
        let url = reqwest::Url::parse(&attachment.private_url)?;
        if url.scheme() != "https"
            || !url
                .host_str()
                .is_some_and(|host| host == "slack.com" || host.ends_with(".slack.com"))
        {
            anyhow::bail!("Slack attachment URL is not authorized");
        }
        let token = consume_connector_credential(
            state,
            binding.binding_id,
            SLACK_CONNECTOR_ALIAS,
            SLACK_DOWNLOAD_OPERATION,
        )
        .await?;
        let response = state.http.get(url).bearer_auth(token).send().await?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > attachment.size_bytes || n > binding.maximum_attachment_bytes)
        {
            anyhow::bail!("Slack attachment download failed or exceeded its bound");
        }
        let bytes = response.bytes().await?;
        if bytes.len() as u64 != attachment.size_bytes {
            anyhow::bail!("Slack attachment size differs from signed metadata");
        }
        let digest = sha256_digest(&bytes);
        let scan = state
            .http
            .post(scanner.clone())
            .bearer_auth(scanner_token.as_str())
            .header("x-content-sha256", &digest)
            .header("x-media-type", &attachment.media_type)
            .body(bytes)
            .send()
            .await?;
        if !scan.status().is_success() || scan.content_length().is_some_and(|n| n > 64 * 1024) {
            anyhow::bail!("attachment scanner failed");
        }
        let receipt: Value = scan.json().await?;
        if receipt.get("clean").and_then(Value::as_bool) != Some(true)
            || receipt.get("contentDigest").and_then(Value::as_str) != Some(digest.as_str())
        {
            anyhow::bail!("attachment scanner rejected content or returned a mismatched digest");
        }
        let immutable = receipt
            .get("immutableReference")
            .and_then(Value::as_str)
            .context("scanner omitted immutable reference")?;
        let scanner_id = receipt
            .get("scannerId")
            .and_then(Value::as_str)
            .context("scanner omitted identity")?;
        let receipt_digest = sha256_digest(&serde_json::to_vec(&receipt)?);
        sqlx::query("INSERT INTO agent_channel_attachment_t(host_id,attachment_id,message_id,external_file_id,media_type,size_bytes,content_digest,immutable_reference,scanner_id,scanner_receipt_digest,scan_state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'CLEAN') ON CONFLICT(host_id,message_id,external_file_id) DO NOTHING")
          .bind(state.host_id).bind(Uuid::now_v7()).bind(message_id).bind(&attachment.external_file_id).bind(&attachment.media_type).bind(attachment.size_bytes as i64).bind(&digest).bind(immutable).bind(scanner_id).bind(receipt_digest).execute(&state.pool).await?;
        references.push(format!(
            "{} ({}, {})",
            immutable, attachment.media_type, digest
        ));
    }
    Ok(references)
}

async fn consume_connector_credential(
    state: &AppState,
    binding_id: Uuid,
    connector_alias: &str,
    operation: &str,
) -> Result<String> {
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT grant_id,credential_reference FROM agent_connector_grant_t
         WHERE host_id=$1 AND binding_id=$2 AND connector_alias=$3
           AND revoked_ts IS NULL AND expires_ts>now() AND use_count<maximum_uses
           AND allowed_operations ? $4
         ORDER BY created_ts DESC LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .bind(state.host_id)
    .bind(binding_id)
    .bind(connector_alias)
    .bind(operation)
    .fetch_optional(&mut *tx)
    .await?
    .context("no live connector grant authorizes the operation")?;
    let reference: String = row.try_get("credential_reference")?;
    let token = state
        .connector_credentials
        .bearer(&reference, connector_alias)
        .map_err(anyhow::Error::msg)?
        .to_string();
    let consumed = sqlx::query(
        "UPDATE agent_connector_grant_t SET use_count=use_count+1
         WHERE host_id=$1 AND grant_id=$2 AND revoked_ts IS NULL
           AND expires_ts>now() AND use_count<maximum_uses AND allowed_operations ? $3",
    )
    .bind(state.host_id)
    .bind(row.try_get::<Uuid, _>("grant_id")?)
    .bind(operation)
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() != 1 {
        anyhow::bail!("connector grant changed during credential admission");
    }
    tx.commit().await?;
    Ok(token)
}

async fn attachment_recovery_loop(state: AppState) {
    loop {
        if let Err(error) = attachment_recovery_pass(&state).await {
            tracing::warn!(%error,"attachment recovery pass failed");
        }
        tokio::time::sleep(StdDuration::from_secs(5)).await;
    }
}

async fn attachment_recovery_pass(state: &AppState) -> Result<()> {
    let row=sqlx::query("SELECT m.message_id,m.external_event_id,m.payload,b.binding_id,b.principal_id,b.agent_def_id,b.adapter_id,b.external_identity,b.allowed_destinations,b.group_allowed,b.maximum_attachment_bytes,b.quiet_hours,b.revoked_ts
      FROM agent_channel_message_t m JOIN agent_channel_binding_t b ON b.host_id=m.host_id AND b.binding_id=m.binding_id
      WHERE m.host_id=$1 AND m.direction='INBOUND' AND m.state='RECEIVED' AND m.payload->>'provider'='slack'
       AND jsonb_array_length(COALESCE(m.payload->'attachments','[]'::jsonb))>0 AND m.created_ts<now()-interval '5 seconds'
      ORDER BY m.created_ts LIMIT 1").bind(state.host_id).fetch_optional(&state.pool).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let message_id: Uuid = row.try_get("message_id")?;
    let payload: Value = row.try_get("payload")?;
    let quiet: Value = row.try_get("quiet_hours")?;
    let binding = ChannelBinding {
        binding_id: row.try_get("binding_id")?,
        host_id: state.host_id,
        principal_id: row.try_get("principal_id")?,
        agent_def_id: row.try_get("agent_def_id")?,
        adapter_id: row.try_get("adapter_id")?,
        external_identity: row.try_get("external_identity")?,
        allowed_destinations: serde_json::from_value(row.try_get("allowed_destinations")?)?,
        group_allowed: row.try_get("group_allowed")?,
        maximum_attachment_bytes: row.try_get::<i64, _>("maximum_attachment_bytes")? as u64,
        quiet_start_hour: quiet.get("startHour").and_then(Value::as_u64).unwrap_or(22) as u8,
        quiet_end_hour: quiet.get("endHour").and_then(Value::as_u64).unwrap_or(7) as u8,
        revoked_at: row.try_get("revoked_ts")?,
    };
    let attachments: Vec<slack::SlackAttachment> = serde_json::from_value(
        payload
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )?;
    let external_event_id: String = row.try_get("external_event_id")?;
    let event_id = payload
        .get("eventId")
        .and_then(Value::as_str)
        .unwrap_or(&external_event_id)
        .to_string();
    let mut text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match scan_slack_attachments(state, &binding, message_id, &attachments).await {
        Ok(references) => {
            text.push_str("\n\nApproved scanned attachments:\n");
            for reference in references {
                text.push_str(&format!("- {reference}\n"));
            }
            admit_channel_turn(state, &binding, &event_id, &text, message_id).await?;
        }
        Err(error) => {
            sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','attachment_rejected','message',$1),updated_ts=now() WHERE host_id=$2 AND message_id=$3 AND state='RECEIVED'").bind(error.to_string()).bind(state.host_id).bind(message_id).execute(&state.pool).await?;
        }
    }
    Ok(())
}

async fn admit_channel_turn(
    state: &AppState,
    binding: &ChannelBinding,
    event_id: &str,
    text: &str,
    message_id: Uuid,
) -> Result<()> {
    let definition=sqlx::query("SELECT d.aggregate_version,d.policy_snapshot_id,d.model_provider,d.model_name,
            p.resolved_snapshot FROM agent_definition_t d JOIN agent_policy_snapshot_t p
              ON p.host_id=d.host_id AND p.policy_snapshot_id=d.policy_snapshot_id AND p.revoked_ts IS NULL
            WHERE d.host_id=$1 AND d.agent_def_id=$2")
        .bind(state.host_id).bind(binding.agent_def_id).fetch_one(&state.pool).await?;
    let policy: PolicySnapshot = serde_json::from_value(definition.try_get("resolved_snapshot")?)?;
    let session = AgentSessionId(binding.binding_id);
    state
        .repository
        .create_or_resume_session(&SessionSpec {
            host_id: state.host_id,
            session_id: session,
            principal_id: binding.principal_id.clone(),
            user_id: None,
            agent_def_id: binding.agent_def_id,
            bank_id: None,
            policy,
            idle_expires_at: Utc::now() + Duration::hours(24),
            maximum_expires_at: Utc::now() + Duration::days(30),
            resume_handle_digest: sha256_digest(format!("slack:{}", binding.binding_id).as_bytes()),
        })
        .await?;
    let turn = state
        .repository
        .admit_user_turn(state.host_id, session, event_id, text)
        .await?;
    sqlx::query("UPDATE agent_turn_t SET origin_kind='channel',origin_ref=$1 WHERE host_id=$2 AND turn_id=$3")
        .bind(message_id.to_string()).bind(state.host_id).bind(turn.turn_id.0).execute(&state.pool).await?;
    sqlx::query(
        "UPDATE agent_channel_message_t SET state='TURN_CREATED',turn_id=$1,updated_ts=now()
        WHERE host_id=$2 AND message_id=$3 AND state='RECEIVED'",
    )
    .bind(turn.turn_id.0)
    .bind(state.host_id)
    .bind(message_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn trigger_loop(state: AppState) {
    loop {
        if let Err(error) = trigger_pass(&state).await {
            tracing::warn!(%error,"personal trigger pass failed");
        }
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }
}

async fn trigger_pass(state: &AppState) -> Result<()> {
    let mut tx = state.pool.begin().await?;
    let row=sqlx::query("SELECT t.trigger_id,t.binding_id,t.trigger_kind,t.schedule_or_cursor,t.maximum_delay_seconds,t.next_fire_ts,
      b.principal_id,b.agent_def_id,b.adapter_id,b.external_identity,b.allowed_destinations,b.group_allowed,b.maximum_attachment_bytes,b.quiet_hours,b.revoked_ts
      FROM agent_trigger_t t JOIN agent_channel_binding_t b ON b.host_id=t.host_id AND b.binding_id=t.binding_id
      LEFT JOIN agent_connector_grant_t g ON g.host_id=t.host_id AND g.grant_id=t.connector_grant_id
      WHERE t.host_id=$1 AND t.state='ACTIVE' AND t.next_fire_ts<=now() AND b.revoked_ts IS NULL
       AND (t.trigger_kind='SCHEDULE' OR (g.revoked_ts IS NULL AND g.expires_ts>now() AND g.use_count<g.maximum_uses AND g.allowed_operations ? 'triggers.fire'))
      ORDER BY t.next_fire_ts LIMIT 1 FOR UPDATE OF t SKIP LOCKED").bind(state.host_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(());
    };
    let trigger_id: Uuid = row.try_get("trigger_id")?;
    let binding_id: Uuid = row.try_get("binding_id")?;
    let spec: Value = row.try_get("schedule_or_cursor")?;
    let due: chrono::DateTime<Utc> = row.try_get("next_fire_ts")?;
    let max_delay: i32 = row.try_get("maximum_delay_seconds")?;
    let interval = spec
        .get("intervalSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(3600)
        .clamp(60, 86400);
    let key = format!("trigger:{trigger_id}:{}", due.timestamp());
    sqlx::query("UPDATE agent_trigger_t SET last_fire_ts=now(),last_idempotency_key=$1,fire_count=fire_count+1,next_fire_ts=$2+make_interval(secs=>$3) WHERE host_id=$4 AND trigger_id=$5")
      .bind(&key).bind(due).bind(interval as i32).bind(state.host_id).bind(trigger_id).execute(&mut *tx).await?;
    if row.try_get::<String, _>("trigger_kind")? == "CONNECTOR" {
        let consumed=sqlx::query("UPDATE agent_connector_grant_t SET use_count=use_count+1 WHERE host_id=$1 AND grant_id=(SELECT connector_grant_id FROM agent_trigger_t WHERE host_id=$1 AND trigger_id=$2) AND revoked_ts IS NULL AND expires_ts>now() AND use_count<maximum_uses AND allowed_operations ? 'triggers.fire'").bind(state.host_id).bind(trigger_id).execute(&mut *tx).await?;
        if consumed.rows_affected() != 1 {
            sqlx::query(
                "UPDATE agent_trigger_t SET state='PAUSED' WHERE host_id=$1 AND trigger_id=$2",
            )
            .bind(state.host_id)
            .bind(trigger_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    if Utc::now() > due + Duration::seconds(max_delay as i64) {
        tx.commit().await?;
        return Ok(());
    }
    let destination = spec
        .get("destination")
        .and_then(Value::as_str)
        .context("trigger destination missing")?
        .to_string();
    let text = spec
        .get("text")
        .and_then(Value::as_str)
        .context("trigger text missing")?
        .to_string();
    let message_id = Uuid::now_v7();
    let inserted=sqlx::query("INSERT INTO agent_channel_message_t(host_id,message_id,binding_id,external_event_id,response_destination,direction,payload_digest,state,payload) VALUES($1,$2,$3,$4,$5,'INBOUND',$6,'RECEIVED',$7) ON CONFLICT(host_id,binding_id,external_event_id,direction) DO NOTHING")
      .bind(state.host_id).bind(message_id).bind(binding_id).bind(&key).bind(&destination).bind(sha256_digest(text.as_bytes())).bind(json!({"text":text,"provider":"trigger"})).execute(&mut *tx).await?;
    tx.commit().await?;
    if inserted.rows_affected() == 0 {
        return Ok(());
    }
    let quiet: Value = row.try_get("quiet_hours")?;
    let binding = ChannelBinding {
        binding_id,
        host_id: state.host_id,
        principal_id: row.try_get("principal_id")?,
        agent_def_id: row.try_get("agent_def_id")?,
        adapter_id: row.try_get("adapter_id")?,
        external_identity: row.try_get("external_identity")?,
        allowed_destinations: serde_json::from_value(row.try_get("allowed_destinations")?)?,
        group_allowed: row.try_get("group_allowed")?,
        maximum_attachment_bytes: row.try_get::<i64, _>("maximum_attachment_bytes")? as u64,
        quiet_start_hour: quiet.get("startHour").and_then(Value::as_u64).unwrap_or(22) as u8,
        quiet_end_hour: quiet.get("endHour").and_then(Value::as_u64).unwrap_or(7) as u8,
        revoked_at: None,
    };
    if !binding.allowed_destinations.contains(&destination)
        || light_agent_channel::quiet_hours(&binding, Utc::now())
    {
        sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','trigger_policy_denied'),updated_ts=now() WHERE host_id=$1 AND message_id=$2 AND state='RECEIVED'")
            .bind(state.host_id).bind(message_id).execute(&state.pool).await?;
        return Ok(());
    }
    admit_channel_turn(state, &binding, &key, &text, message_id).await
}

async fn delivery_loop(state: AppState) {
    loop {
        if let Err(e) = delivery_pass(&state).await {
            tracing::warn!(%e,"Slack delivery pass failed");
        }
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }
}

async fn delivery_pass(state: &AppState) -> Result<()> {
    sqlx::query("INSERT INTO agent_channel_message_t(host_id,message_id,binding_id,external_event_id,
        response_destination,direction,payload_digest,state,turn_id,payload,connector_grant_id,connector_data_boundary_digest)
      SELECT m.host_id,gen_random_uuid(),m.binding_id,'reply:'||m.external_event_id,m.response_destination,
        'OUTBOUND',encode(digest(convert_to(t.terminal_result::text,'UTF8'),'sha256'),'hex'),'PENDING_DELIVERY',
        t.turn_id,jsonb_build_object('text',COALESCE(t.terminal_result->>'text',t.terminal_result::text)),
        g.grant_id,g.data_boundary_digest
      FROM agent_channel_message_t m JOIN agent_turn_t t ON t.host_id=m.host_id AND t.turn_id=m.turn_id
      JOIN agent_channel_binding_t b ON b.host_id=m.host_id AND b.binding_id=m.binding_id AND b.adapter_id='slack-events-v1'
      JOIN LATERAL(SELECT grant_id,data_boundary_digest FROM agent_connector_grant_t
        WHERE host_id=m.host_id AND binding_id=m.binding_id AND connector_alias='slack-api-v1'
          AND revoked_ts IS NULL AND expires_ts>now() AND use_count<maximum_uses
          AND allowed_operations ? 'chat.postMessage'
        ORDER BY created_ts DESC LIMIT 1) g ON TRUE
      WHERE m.direction='INBOUND' AND m.state='TURN_CREATED' AND t.state='COMPLETED'
      ON CONFLICT(host_id,binding_id,external_event_id,direction) DO NOTHING").execute(&state.pool).await?;
    sqlx::query(
        "UPDATE agent_channel_message_t SET state='FAILED',next_attempt_ts=now(),
      last_error=jsonb_build_object('class','delivery_claim_recovered'),updated_ts=now()
      WHERE direction='OUTBOUND' AND state='SENDING' AND updated_ts<now()-interval '2 minutes'",
    )
    .execute(&state.pool)
    .await?;
    let mut tx = state.pool.begin().await?;
    let row=sqlx::query("SELECT m.host_id,m.message_id,m.response_destination,m.payload,m.attempt_count,b.quiet_hours,b.revoked_ts,
        g.grant_id,g.connector_alias,g.allowed_operations,g.data_boundary_digest,g.credential_reference,g.expires_ts,g.revoked_ts AS grant_revoked_ts
      FROM agent_channel_message_t m JOIN agent_channel_binding_t b ON b.host_id=m.host_id AND b.binding_id=m.binding_id
      JOIN agent_connector_grant_t g ON g.host_id=m.host_id AND g.grant_id=m.connector_grant_id
        AND g.data_boundary_digest=m.connector_data_boundary_digest
      WHERE m.host_id=$1 AND m.direction='OUTBOUND' AND m.state IN('PENDING_DELIVERY','FAILED')
        AND (m.next_attempt_ts IS NULL OR m.next_attempt_ts<=now()) ORDER BY m.created_ts LIMIT 1 FOR UPDATE OF m SKIP LOCKED")
        .bind(state.host_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(());
    };
    let id: Uuid = row.try_get("message_id")?;
    if row
        .try_get::<Option<chrono::DateTime<Utc>>, _>("revoked_ts")?
        .is_some()
    {
        sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','binding_revoked'),updated_ts=now() WHERE host_id=$1 AND message_id=$2")
            .bind(state.host_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    if row.try_get::<i32, _>("attempt_count")? >= 10 {
        sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','retry_exhausted'),updated_ts=now() WHERE host_id=$1 AND message_id=$2")
            .bind(state.host_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let quiet: Value = row.try_get("quiet_hours")?;
    let hour = Utc::now().hour() as u64;
    let start = quiet.get("startHour").and_then(Value::as_u64).unwrap_or(22);
    let end = quiet.get("endHour").and_then(Value::as_u64).unwrap_or(7);
    if (start <= end && hour >= start && hour < end)
        || (start > end && (hour >= start || hour < end))
    {
        sqlx::query("UPDATE agent_channel_message_t SET next_attempt_ts=now()+interval '15 minutes',updated_ts=now() WHERE host_id=$1 AND message_id=$2")
            .bind(state.host_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let payload: Value = row.try_get("payload")?;
    let destination: String = row.try_get("response_destination")?;
    let grant_id: Uuid = row.try_get("grant_id")?;
    let connector_alias: String = row.try_get("connector_alias")?;
    let data_boundary_digest: String = row.try_get("data_boundary_digest")?;
    let operations: Value = row.try_get("allowed_operations")?;
    let credential_reference: String = row.try_get("credential_reference")?;
    let grant_invalid = connector_alias != SLACK_CONNECTOR_ALIAS
        || row
            .try_get::<Option<chrono::DateTime<Utc>>, _>("grant_revoked_ts")?
            .is_some()
        || row.try_get::<chrono::DateTime<Utc>, _>("expires_ts")? <= Utc::now()
        || operations.as_array().is_none_or(|values| {
            !values
                .iter()
                .any(|v| v.as_str() == Some(SLACK_POST_MESSAGE_OPERATION))
        });
    if grant_invalid {
        sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','connector_grant_invalid'),updated_ts=now() WHERE host_id=$1 AND message_id=$2")
            .bind(state.host_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let token = match state
        .connector_credentials
        .bearer(&credential_reference, &connector_alias)
    {
        Ok(token) => token.to_string(),
        Err(_) => {
            sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','connector_credential_unavailable'),updated_ts=now() WHERE host_id=$1 AND message_id=$2")
                .bind(state.host_id).bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
    };
    let consumed = sqlx::query(
        "UPDATE agent_connector_grant_t SET use_count=use_count+1
      WHERE host_id=$1 AND grant_id=$2 AND connector_alias=$3 AND data_boundary_digest=$4
        AND revoked_ts IS NULL AND expires_ts>now() AND use_count<maximum_uses
        AND allowed_operations ? 'chat.postMessage'",
    )
    .bind(state.host_id)
    .bind(grant_id)
    .bind(&connector_alias)
    .bind(&data_boundary_digest)
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() != 1 {
        sqlx::query("UPDATE agent_channel_message_t SET state='REJECTED',last_error=jsonb_build_object('class','connector_grant_exhausted'),updated_ts=now() WHERE host_id=$1 AND message_id=$2")
            .bind(state.host_id).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    sqlx::query("UPDATE agent_channel_message_t SET state='SENDING',attempt_count=attempt_count+1,
      updated_ts=now() WHERE host_id=$1 AND message_id=$2 AND state IN('PENDING_DELIVERY','FAILED')")
        .bind(state.host_id).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    let response=state.http.post("https://slack.com/api/chat.postMessage").bearer_auth(token)
        .json(&json!({"channel":destination,"text":payload.get("text").and_then(Value::as_str).unwrap_or(""),"client_msg_id":id})).send().await;
    match response {
        Ok(response) if response.status().is_success() => {
            let receipt: Value = response.json().await?;
            if receipt.get("ok").and_then(Value::as_bool) == Some(true) {
                sqlx::query("UPDATE agent_channel_message_t SET state='DELIVERED',receipt=$1,provider_receipt_id=$2,updated_ts=now() WHERE host_id=$3 AND message_id=$4 AND state='SENDING'")
                .bind(&receipt).bind(receipt.get("ts").and_then(Value::as_str)).bind(state.host_id).bind(id).execute(&state.pool).await?;
            } else {
                schedule_retry(state, id, json!({"provider":receipt})).await?;
            }
        }
        Ok(response) => {
            schedule_retry(state, id, json!({"httpStatus":response.status().as_u16()})).await?
        }
        Err(error) => schedule_retry(state, id, json!({"transport":error.to_string()})).await?,
    }
    Ok(())
}
async fn schedule_retry(state: &AppState, id: Uuid, error: Value) -> Result<()> {
    sqlx::query("UPDATE agent_channel_message_t SET state='FAILED',
    next_attempt_ts=now()+LEAST(interval '5 minutes',make_interval(secs=>power(2,LEAST(attempt_count,8))::int)),last_error=$1,updated_ts=now()
    WHERE host_id=$2 AND message_id=$3 AND state='SENDING'").bind(error).bind(state.host_id).bind(id).execute(&state.pool).await?;
    Ok(())
}
