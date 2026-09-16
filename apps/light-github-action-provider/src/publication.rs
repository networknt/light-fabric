//! Issue/comment/document delivery with a durable intent before the only write.
//! Status and repeated delivery perform GET requests only.
use super::{AppState, authenticated, bounded_json, github};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use development_workflow_contract::publication::*;
use reqwest::Method;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS publication_journal(key TEXT PRIMARY KEY,request TEXT NOT NULL,receipt TEXT);")?;
    Ok(())
}

pub async fn execute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PublicationDelivery>,
) -> Response {
    handle(state, headers, request, true).await
}

pub async fn inspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PublicationDelivery>,
) -> Response {
    handle(state, headers, request, false).await
}

async fn handle(
    state: AppState,
    headers: HeaderMap,
    request: PublicationDelivery,
    dispatch: bool,
) -> Response {
    if !authenticated(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match deliver(&state, &request, dispatch).await {
        Ok(Some(receipt)) => Json(receipt).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "publication unresolved or rejected",
        )
            .into_response(),
    }
}

fn validate(state: &AppState, request: &PublicationDelivery) -> Result<()> {
    let policy = state
        .publication_policy
        .as_ref()
        .context("publication disabled")?;
    request
        .plan
        .validate(policy, &request.plan.candidate_digest)?;
    ensure!(
        request.key.len() == 64 && request.key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid effect key"
    );
    ensure!(
        !request.body.is_empty()
            && request.body.len() <= 64 * 1024
            && !request.body.contains("<!-- light-workflow-publication:"),
        "invalid publication body"
    );
    if let Destination::Document { expected_blob, .. } = &request.plan.destination {
        ensure!(
            expected_blob.is_none(),
            "immutable document publication cannot replace a blob"
        );
    }
    Ok(())
}

fn marked_body(request: &PublicationDelivery) -> Result<String> {
    Ok(format!(
        "{}\n\n<!-- light-workflow-publication:{}:{} -->\n",
        request.body,
        request.key,
        request.request_digest()?
    ))
}

pub(super) async fn deliver(
    state: &AppState,
    request: &PublicationDelivery,
    dispatch: bool,
) -> Result<Option<ProviderPublicationReceipt>> {
    validate(state, request)?;
    let encoded = serde_json::to_string(request)?;
    let fresh = {
        let db = state.db.lock().await;
        let saved: Option<(String, Option<String>)> = db
            .query_row(
                "SELECT request,receipt FROM publication_journal WHERE key=?1",
                [&request.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old, receipt)) = saved {
            ensure!(old == encoded, "publication request conflict");
            if let Some(receipt) = receipt {
                return Ok(Some(serde_json::from_str(&receipt)?));
            }
            false
        } else if dispatch {
            db.execute(
                "INSERT INTO publication_journal(key,request) VALUES(?1,?2)",
                params![request.key, encoded],
            )?;
            true
        } else {
            // A manager intent may have committed before reaching this provider.
            // Inspect remote markers but do not create a provider intent or write.
            false
        }
    };
    let observed = observe(state, request).await?;
    let receipt = if observed.is_some() {
        observed
    } else if fresh {
        write_once(state, request).await?;
        observe(state, request).await?
    } else {
        None
    };
    if let Some(receipt) = &receipt {
        let db = state.db.lock().await;
        let encoded_receipt = serde_json::to_string(receipt)?;
        let old: Option<String> = db
            .query_row(
                "SELECT receipt FROM publication_journal WHERE key=?1 AND request=?2",
                params![request.key, encoded],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        ensure!(
            old.as_ref().is_none_or(|old| old == &encoded_receipt),
            "confirmation conflict"
        );
        db.execute("UPDATE publication_journal SET receipt=?3 WHERE key=?1 AND request=?2 AND receipt IS NULL", params![request.key, encoded, encoded_receipt])?;
    }
    Ok(receipt)
}

async fn write_once(state: &AppState, request: &PublicationDelivery) -> Result<()> {
    let root = format!("repos/{}", request.plan.repository);
    let body = marked_body(request)?;
    let response = match &request.plan.destination {
        Destination::Issue { title } => github(state, Method::POST, &format!("{root}/issues")).await?.json(&json!({"title":title,"body":body})).send().await?,
        Destination::Comment { issue } => github(state, Method::POST, &format!("{root}/issues/{issue}/comments")).await?.json(&json!({"body":body})).send().await?,
        Destination::Document { branch, path, .. } => github(state, Method::PUT, &format!("{root}/contents/{path}")).await?.json(&json!({"branch":branch,"message":format!("Publish Workflow {}",request.key),"content":STANDARD.encode(body)})).send().await?,
    };
    ensure!(
        response.status().is_success(),
        "GitHub publication result unresolved"
    );
    // The response may be lost. Confirmation always comes from an independent read.
    Ok(())
}

async fn observe(
    state: &AppState,
    request: &PublicationDelivery,
) -> Result<Option<ProviderPublicationReceipt>> {
    let root = format!("repos/{}", request.plan.repository);
    let body = marked_body(request)?;
    let mut found = None;
    match &request.plan.destination {
        Destination::Issue { .. } | Destination::Comment { .. } => {
            let user = bounded_json(
                github(state, Method::GET, "user")
                    .await?
                    .send()
                    .await?
                    .error_for_status()?,
            )
            .await?;
            let author = user
                .get("id")
                .and_then(Value::as_u64)
                .context("missing provider account identity")?;
            let endpoint = match &request.plan.destination {
                Destination::Issue { .. } => format!("{root}/issues"),
                Destination::Comment { issue } => format!("{root}/issues/{issue}/comments"),
                _ => unreachable!(),
            };
            // Bounded pagination; reaching the bound leaves the effect unresolved.
            for page in 1..=20 {
                let response = github(state, Method::GET, &endpoint)
                    .await?
                    .query(&[
                        ("state", "all"),
                        ("per_page", "100"),
                        ("page", &page.to_string()),
                    ])
                    .send()
                    .await?
                    .error_for_status()?;
                let values = bounded_json(response).await?;
                let values = values.as_array().context("expected GitHub list")?;
                for value in values {
                    if value.get("body").and_then(Value::as_str) != Some(body.as_str()) {
                        continue;
                    }
                    ensure!(
                        value.pointer("/user/id").and_then(Value::as_u64) == Some(author),
                        "publication author mismatch"
                    );
                    if let Destination::Issue { title } = &request.plan.destination {
                        ensure!(
                            value.get("title").and_then(Value::as_str) == Some(title)
                                && value.get("pull_request").is_none(),
                            "issue binding mismatch"
                        );
                    }
                    ensure!(found.is_none(), "duplicate remote publication markers");
                    let id = value
                        .get("id")
                        .and_then(Value::as_u64)
                        .context("missing resource identity")?;
                    let url = value
                        .get("html_url")
                        .and_then(Value::as_str)
                        .context("missing resource URL")?;
                    ensure!(
                        url.starts_with(&format!(
                            "https://github.com/{}/issues/",
                            request.plan.repository
                        )),
                        "resource escaped repository"
                    );
                    found = Some(receipt(request, id.to_string(), url.to_string(), None)?);
                }
                if values.len() < 100 {
                    break;
                }
                ensure!(page < 20, "publication inspection bound exceeded");
            }
        }
        Destination::Document { branch, path, .. } => {
            let response = github(state, Method::GET, &format!("{root}/contents/{path}"))
                .await?
                .query(&[("ref", branch)])
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let value = bounded_json(response.error_for_status()?).await?;
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .context("missing document content")?;
            ensure!(
                STANDARD.decode(content.replace(['\r', '\n'], ""))? == body.as_bytes(),
                "document exists with different bytes"
            );
            ensure!(
                value.get("path").and_then(Value::as_str) == Some(path)
                    && value.get("type").and_then(Value::as_str) == Some("file"),
                "document path mismatch"
            );
            let commits = bounded_json(
                github(state, Method::GET, &format!("{root}/commits"))
                    .await?
                    .query(&[
                        ("sha", branch.as_str()),
                        ("path", path.as_str()),
                        ("per_page", "1"),
                    ])
                    .send()
                    .await?
                    .error_for_status()?,
            )
            .await?;
            let commit = commits
                .pointer("/0/sha")
                .and_then(Value::as_str)
                .context("missing document commit")?;
            ensure!(
                commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid document commit"
            );
            // Verify the permalink reads the exact content, independent of branch movement.
            let fixed = bounded_json(
                github(state, Method::GET, &format!("{root}/contents/{path}"))
                    .await?
                    .query(&[("ref", commit)])
                    .send()
                    .await?
                    .error_for_status()?,
            )
            .await?;
            ensure!(
                fixed.get("sha") == value.get("sha"),
                "document commit moved"
            );
            found = Some(receipt(
                request,
                value
                    .get("sha")
                    .and_then(Value::as_str)
                    .context("missing blob")?
                    .into(),
                format!(
                    "https://github.com/{}/blob/{commit}/{path}",
                    request.plan.repository
                ),
                Some(commit.into()),
            )?);
        }
    }
    Ok(found)
}

fn receipt(
    request: &PublicationDelivery,
    provider_id: String,
    resource_url: String,
    commit: Option<String>,
) -> Result<ProviderPublicationReceipt> {
    Ok(ProviderPublicationReceipt {
        key: request.key.clone(),
        request_digest: request.request_digest()?,
        provider_id,
        resource_url,
        commit,
    })
}
