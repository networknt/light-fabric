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
use light_agent::domain::{AgentRepository, SessionSpec};
use light_agent_channel::{
    ChannelBinding,
    slack::{self, SlackInbound},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{env, net::SocketAddr, sync::Arc, time::Duration as StdDuration};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    repository: AgentRepository,
    host_id: Uuid,
    signing_secret: Arc<Vec<u8>>,
    bot_token: Arc<String>,
    http: reqwest::Client,
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
    let state = AppState {
        repository: AgentRepository::new(pool.clone()),
        pool,
        host_id,
        signing_secret: Arc::new(secret),
        bot_token: Arc::new(env::var("SLACK_BOT_TOKEN")?),
        http: reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .build()?,
    };
    tokio::spawn(delivery_loop(state.clone()));
    let app = Router::new()
        .route("/channels/slack/events", post(slack_events))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);
    let addr: SocketAddr = env::var("LIGHT_AGENT_CHANNEL_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8440".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
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
        .bind(&message.destination).bind(digest).bind(json!({"text":message.text,"provider":"slack"}))
        .fetch_optional(&state.pool).await?;
    if inserted.is_none() {
        return Ok(None);
    }
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
        .admit_user_turn(
            state.host_id,
            session,
            &message.event_id,
            &message.text,
            definition.try_get("model_provider")?,
            definition.try_get("model_name")?,
        )
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
    Ok(None)
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
        response_destination,direction,payload_digest,state,turn_id,payload)
      SELECT m.host_id,gen_random_uuid(),m.binding_id,'reply:'||m.external_event_id,m.response_destination,
        'OUTBOUND',encode(digest(convert_to(t.terminal_result::text,'UTF8'),'sha256'),'hex'),'PENDING_DELIVERY',
        t.turn_id,jsonb_build_object('text',COALESCE(t.terminal_result->>'text',t.terminal_result::text))
      FROM agent_channel_message_t m JOIN agent_turn_t t ON t.host_id=m.host_id AND t.turn_id=m.turn_id
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
    let row=sqlx::query("SELECT m.host_id,m.message_id,m.response_destination,m.payload,m.attempt_count,b.quiet_hours,b.revoked_ts
      FROM agent_channel_message_t m JOIN agent_channel_binding_t b ON b.host_id=m.host_id AND b.binding_id=m.binding_id
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
    sqlx::query("UPDATE agent_channel_message_t SET state='SENDING',attempt_count=attempt_count+1,
      updated_ts=now() WHERE host_id=$1 AND message_id=$2 AND state IN('PENDING_DELIVERY','FAILED')")
        .bind(state.host_id).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    let response=state.http.post("https://slack.com/api/chat.postMessage").bearer_auth(state.bot_token.as_str())
        .json(&json!({"channel":destination,"text":payload.get("text").and_then(Value::as_str).unwrap_or("")})).send().await;
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
