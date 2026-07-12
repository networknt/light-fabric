use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::Url;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    http: reqwest::Client,
    api: Url,
    service_secret: Arc<Vec<u8>>,
    github_token: Arc<String>,
    repositories: Arc<BTreeMap<String, Repository>>,
    branch_prefix: Arc<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Repository {
    owner: String,
    repo: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionRequest {
    fixed_action_id: uuid::Uuid,
    execution_id: uuid::Uuid,
    approval_id: uuid::Uuid,
    operation: String,
    immutable_input_digest: String,
    target: Value,
    policy_digest: String,
    provenance_digest: Option<String>,
    spec: Value,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    provider_operation_id: String,
    state: String,
    evidence_digest: String,
    resource_reference: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let db_path = PathBuf::from(env::var("GITHUB_ACTION_PROVIDER_DB")?);
    let db = Connection::open(db_path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS operation_journal(idempotency_key TEXT PRIMARY KEY,operation TEXT NOT NULL,request_json TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('IN_FLIGHT','SUCCEEDED','FAILED')),receipt_json TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);")?;
    let service_secret = String::from_utf8(read_secret(&env::var(
        "GITHUB_ACTION_PROVIDER_SERVICE_TOKEN_FILE",
    )?)?)?
    .trim()
    .as_bytes()
    .to_vec();
    let github_token = String::from_utf8(read_secret(&env::var(
        "GITHUB_ACTION_PROVIDER_TOKEN_FILE",
    )?)?)?
    .trim()
    .to_string();
    let repositories: BTreeMap<String, Repository> =
        serde_json::from_str(&env::var("GITHUB_ACTION_PROVIDER_REPOSITORIES")?)?;
    if repositories.is_empty()
        || repositories
            .values()
            .any(|r| !github_name(&r.owner) || !github_name(&r.repo))
        || service_secret.len() < 32
        || github_token.len() < 16
    {
        anyhow::bail!("provider secrets and repository allowlist are required")
    }
    let api = Url::parse(
        &env::var("GITHUB_ACTION_PROVIDER_API_URL")
            .unwrap_or_else(|_| "https://api.github.com/".into()),
    )?;
    if api.scheme() != "https"
        && !api
            .host_str()
            .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost" | "::1"))
    {
        anyhow::bail!("GitHub API URL must use HTTPS")
    }
    let state = AppState {
        db: Arc::new(Mutex::new(db)),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?,
        api,
        service_secret: Arc::new(service_secret),
        github_token: Arc::new(github_token),
        repositories: Arc::new(repositories),
        branch_prefix: Arc::new(
            env::var("GITHUB_ACTION_PROVIDER_BRANCH_PREFIX").unwrap_or_else(|_| "agent/".into()),
        ),
    };
    let app = Router::new()
        .route("/v1/fixed-actions/{operation}", post(execute))
        .route("/v1/fixed-actions/status", get(status))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state);
    let address: SocketAddr = env::var("GITHUB_ACTION_PROVIDER_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8450".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn read_secret(path: &str) -> Result<Vec<u8>> {
    let path = FsPath::new(path);
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        anyhow::bail!("secret must be an owner-only regular file")
    }
    Ok(fs::read(path)?)
}

fn github_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with('/')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

fn authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let Ok(mut expected) = Hmac::<Sha256>::new_from_slice(&state.service_secret) else {
        return false;
    };
    expected.update(b"light-fixed-action-provider");
    let Ok(mut supplied) = Hmac::<Sha256>::new_from_slice(token.as_bytes()) else {
        return false;
    };
    supplied.update(b"light-fixed-action-provider");
    expected
        .verify_slice(&supplied.finalize().into_bytes())
        .is_ok()
}

async fn execute(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ActionRequest>,
) -> Response {
    if !authenticated(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| (16..=255).contains(&v.len()))
    else {
        return (StatusCode::BAD_REQUEST, "missing idempotency key").into_response();
    };
    match execute_inner(&state, key, &operation, request).await {
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(error) => {
            tracing::warn!(%error,"GitHub fixed action rejected");
            (StatusCode::BAD_GATEWAY, "provider operation unavailable").into_response()
        }
    }
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authenticated(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match inspect_key(&state, key).await {
        Ok(Some(receipt)) => (StatusCode::OK, Json(receipt)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::warn!(%error,"GitHub reconciliation failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn execute_inner(
    state: &AppState,
    key: &str,
    operation: &str,
    request: ActionRequest,
) -> Result<Receipt> {
    validate_request(state, operation, &request)?;
    let encoded = serde_json::to_string(&request)?;
    {
        let db = state.db.lock().await;
        let existing: Option<(String,String,String, Option<String>)> = db
            .query_row(
                "SELECT operation,request_json,state,receipt_json FROM operation_journal WHERE idempotency_key=?1",
                [key],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
            )
            .optional()?;
        if let Some((stored_operation, stored_request, stored, receipt)) = existing {
            if stored_operation != operation || stored_request != encoded {
                anyhow::bail!("idempotency key is already bound to different immutable input")
            }
            if let Some(receipt) = receipt {
                return Ok(serde_json::from_str(&receipt)?);
            }
            if stored == "IN_FLIGHT" {
                drop(db);
                return inspect_key(state, key)
                    .await?
                    .context("operation outcome is still pending");
            }
        }
        db.execute("INSERT INTO operation_journal(idempotency_key,operation,request_json,state,created_at,updated_at) VALUES(?1,?2,?3,'IN_FLIGHT',?4,?4)",params![key,operation,encoded,chrono::Utc::now().to_rfc3339()])?;
    }
    let receipt = reconcile_request(state, &request)
        .await?
        .context("GitHub did not expose authoritative operation evidence")?;
    persist_receipt(state, key, &receipt).await?;
    Ok(receipt)
}

fn validate_request(state: &AppState, operation: &str, r: &ActionRequest) -> Result<()> {
    if operation != r.operation
        || !matches!(operation, "create-branch" | "open-pr")
        || !r.immutable_input_digest.starts_with("sha256:")
        || !r.policy_digest.starts_with("sha256:")
    {
        anyhow::bail!("request binding is invalid")
    }
    let repository = r
        .spec
        .get("repository")
        .and_then(Value::as_str)
        .context("repository missing")?;
    if !state.repositories.contains_key(repository) || r.target.as_str() != Some(repository) {
        anyhow::bail!("repository is not allowlisted or target-bound")
    }
    let branch = r
        .spec
        .get("targetBranch")
        .and_then(Value::as_str)
        .context("targetBranch missing")?;
    if !branch.starts_with(state.branch_prefix.as_str()) || !git_ref(branch) {
        anyhow::bail!("branch is outside the configured prefix")
    }
    if r.spec.get("operation").and_then(Value::as_str) != Some(operation)
        || r.spec.get("patchDigest").and_then(Value::as_str) != Some(&r.immutable_input_digest)
        || r.spec.get("policyDigest").and_then(Value::as_str) != Some(&r.policy_digest)
        || r.spec.get("provenanceDigest").and_then(Value::as_str) != r.provenance_digest.as_deref()
    {
        anyhow::bail!("typed specification differs from request")
    };
    Ok(())
}

async fn github(
    state: &AppState,
    method: reqwest::Method,
    path: &str,
) -> Result<reqwest::RequestBuilder> {
    let url = state.api.join(path)?;
    if url.origin() != state.api.origin() {
        anyhow::bail!("GitHub path escaped configured origin")
    }
    Ok(state
        .http
        .request(method, url)
        .bearer_auth(state.github_token.as_str())
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .header("user-agent", "light-github-action-provider"))
}

async fn bounded_json(response: reqwest::Response) -> Result<Value> {
    const MAX: usize = 256 * 1024;
    if response.content_length().is_some_and(|n| n > MAX as u64) {
        anyhow::bail!("GitHub response exceeds 256 KiB")
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().checked_add(chunk.len()).is_none_or(|n| n > MAX) {
            anyhow::bail!("GitHub response exceeds 256 KiB")
        }
        bytes.extend_from_slice(&chunk)
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn reconcile_request(state: &AppState, r: &ActionRequest) -> Result<Option<Receipt>> {
    let repository = state
        .repositories
        .get(r.spec.get("repository").and_then(Value::as_str).unwrap())
        .unwrap();
    let branch = r.spec.get("targetBranch").and_then(Value::as_str).unwrap();
    match r.operation.as_str() {
        "create-branch" => {
            let base = r
                .spec
                .get("baseCommit")
                .and_then(Value::as_str)
                .context("baseCommit missing")?;
            if !matches!(base.len(), 40 | 64) || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
                anyhow::bail!("baseCommit is invalid")
            }
            let response = github(
                state,
                reqwest::Method::POST,
                &format!("repos/{}/{}/git/refs", repository.owner, repository.repo),
            )
            .await?
            .json(&json!({"ref":format!("refs/heads/{branch}"),"sha":base}))
            .send()
            .await;
            if response.as_ref().is_ok_and(|v| v.status().is_success()) {
                return Ok(Some(receipt(
                    format!("ref:{}/{}/{}", repository.owner, repository.repo, branch),
                    "SUCCEEDED",
                    format!(
                        "https://github.com/{}/{}/tree/{branch}",
                        repository.owner, repository.repo
                    ),
                    json!({"branch":branch,"sha":base}),
                )?));
            }
            inspect_branch(state, repository, branch, base).await
        }
        "open-pr" => {
            let base = r
                .spec
                .get("baseBranch")
                .and_then(Value::as_str)
                .context("baseBranch missing")?;
            if !git_ref(base) {
                anyhow::bail!("baseBranch is invalid")
            }
            let title = r
                .spec
                .get("title")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= 256)
                .context("title missing")?;
            let body = r.spec.get("body").and_then(Value::as_str).unwrap_or("");
            if body.len() > 64 * 1024 {
                anyhow::bail!("pull request body is too large")
            }
            let response = github(
                state,
                reqwest::Method::POST,
                &format!("repos/{}/{}/pulls", repository.owner, repository.repo),
            )
            .await?
            .json(&json!({"head":branch,"base":base,"title":title,"body":body}))
            .send()
            .await;
            if let Ok(response) = response
                && response.status().is_success()
            {
                let value = bounded_json(response).await?;
                return Ok(Some(receipt(
                    format!(
                        "pr:{}/{}#{}",
                        repository.owner,
                        repository.repo,
                        value
                            .get("number")
                            .and_then(Value::as_u64)
                            .context("GitHub PR number missing")?
                    ),
                    "SUCCEEDED",
                    value
                        .get("html_url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    value,
                )?));
            }
            inspect_pr(state, repository, branch, base).await
        }
        _ => anyhow::bail!("unsupported operation"),
    }
}

async fn inspect_branch(
    state: &AppState,
    repo: &Repository,
    branch: &str,
    base: &str,
) -> Result<Option<Receipt>> {
    let response = github(
        state,
        reqwest::Method::GET,
        &format!("repos/{}/{}/git/ref/heads/{branch}", repo.owner, repo.repo),
    )
    .await?
    .send()
    .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        anyhow::bail!("GitHub branch inspection failed")
    };
    let value = bounded_json(response).await?;
    if value.pointer("/object/sha").and_then(Value::as_str) != Some(base) {
        anyhow::bail!("existing branch differs from approved base")
    };
    Ok(Some(receipt(
        format!("ref:{}/{}/{}", repo.owner, repo.repo, branch),
        "SUCCEEDED",
        format!(
            "https://github.com/{}/{}/tree/{branch}",
            repo.owner, repo.repo
        ),
        value,
    )?))
}

async fn inspect_pr(
    state: &AppState,
    repo: &Repository,
    branch: &str,
    base: &str,
) -> Result<Option<Receipt>> {
    let response = github(
        state,
        reqwest::Method::GET,
        &format!("repos/{}/{}/pulls", repo.owner, repo.repo),
    )
    .await?
    .query(&[
        ("state", "all"),
        ("head", &format!("{}:{branch}", repo.owner)),
        ("base", base),
    ])
    .send()
    .await?;
    if !response.status().is_success() {
        anyhow::bail!("GitHub pull request inspection failed")
    };
    let values = bounded_json(response).await?;
    let Some(value) = values.as_array().and_then(|values| values.first()).cloned() else {
        return Ok(None);
    };
    Ok(Some(receipt(
        format!(
            "pr:{}/{}#{}",
            repo.owner,
            repo.repo,
            value
                .get("number")
                .and_then(Value::as_u64)
                .context("GitHub PR number missing")?
        ),
        "SUCCEEDED",
        value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        value,
    )?))
}

fn receipt(id: String, state: &str, resource: String, evidence: Value) -> Result<Receipt> {
    let digest = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(serde_json::to_vec(&evidence)?))
    );
    Ok(Receipt {
        provider_operation_id: id,
        state: state.into(),
        evidence_digest: digest,
        resource_reference: (!resource.is_empty()).then_some(resource),
    })
}
async fn persist_receipt(state: &AppState, key: &str, receipt: &Receipt) -> Result<()> {
    let db = state.db.lock().await;
    db.execute("UPDATE operation_journal SET state=?1,receipt_json=?2,updated_at=?3 WHERE idempotency_key=?4 AND state='IN_FLIGHT'",params![receipt.state,serde_json::to_string(receipt)?,chrono::Utc::now().to_rfc3339(),key])?;
    Ok(())
}
async fn inspect_key(state: &AppState, key: &str) -> Result<Option<Receipt>> {
    let stored: Option<(String, Option<String>)> = {
        let db = state.db.lock().await;
        db.query_row(
            "SELECT request_json,receipt_json FROM operation_journal WHERE idempotency_key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    };
    let Some((request, receipt)) = stored else {
        return Ok(None);
    };
    if let Some(receipt) = receipt {
        return Ok(Some(serde_json::from_str(&receipt)?));
    }
    let request: ActionRequest = serde_json::from_str(&request)?;
    let result = reconcile_request(state, &request).await?;
    if let Some(receipt) = &result {
        persist_receipt(state, key, receipt).await?
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn lost_create_response_is_reconciled_and_never_mutated_twice() {
        let posts = Arc::new(AtomicUsize::new(0));
        let gets = Arc::new(AtomicUsize::new(0));
        let post_count = posts.clone();
        let get_count = gets.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/repos/o/r/git/refs", post(move || {
                        let count = post_count.clone();
                        async move { count.fetch_add(1, Ordering::SeqCst); StatusCode::INTERNAL_SERVER_ERROR }
                    }))
                    .route("/repos/o/r/git/ref/heads/agent/change", get(move || {
                        let count = get_count.clone();
                        async move { count.fetch_add(1, Ordering::SeqCst); Json(json!({"object":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}})) }
                    })),
            ).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("journal.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE operation_journal(idempotency_key TEXT PRIMARY KEY,operation TEXT NOT NULL,request_json TEXT NOT NULL,state TEXT NOT NULL,receipt_json TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);").unwrap();
        let state = AppState {
            db: Arc::new(Mutex::new(db)),
            http: reqwest::Client::new(),
            api: Url::parse(&format!("http://{address}/")).unwrap(),
            service_secret: Arc::new(vec![b's'; 32]),
            github_token: Arc::new("github-token-123456789".into()),
            repositories: Arc::new(BTreeMap::from([(
                "https://github.com/o/r.git".into(),
                Repository {
                    owner: "o".into(),
                    repo: "r".into(),
                },
            )])),
            branch_prefix: Arc::new("agent/".into()),
        };
        let request = ActionRequest {
            fixed_action_id: uuid::Uuid::now_v7(),
            execution_id: uuid::Uuid::now_v7(),
            approval_id: uuid::Uuid::now_v7(),
            operation: "create-branch".into(),
            immutable_input_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            target: json!("https://github.com/o/r.git"),
            policy_digest: "sha256:policy".into(),
            provenance_digest: Some("sha256:provenance".into()),
            spec: json!({"operation":"create-branch","target":"https://github.com/o/r.git","repository":"https://github.com/o/r.git","baseCommit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","targetBranch":"agent/change","patchDigest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","policyDigest":"sha256:policy","provenanceDigest":"sha256:provenance"}),
        };
        let first = execute_inner(
            &state,
            "approval:fixed-action-0001",
            "create-branch",
            request.clone(),
        )
        .await
        .unwrap();
        let replay = execute_inner(
            &state,
            "approval:fixed-action-0001",
            "create-branch",
            request.clone(),
        )
        .await
        .unwrap();
        let mut substituted = request;
        substituted.spec["targetBranch"] = json!("agent/substituted");
        assert!(
            execute_inner(
                &state,
                "approval:fixed-action-0001",
                "create-branch",
                substituted
            )
            .await
            .is_err()
        );
        assert_eq!(first.provider_operation_id, replay.provider_operation_id);
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_eq!(gets.load(Ordering::SeqCst), 1);
    }
}
