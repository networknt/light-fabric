use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use execution_fixed_action::{
    FixedPatchRequest, GitObjectFormat, execute_fixed_patch_in_workspace,
};
use execution_security::ProtectedPathPolicy;
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
    process::Command,
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
    work_root: Arc<PathBuf>,
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
    let work_root = PathBuf::from(env::var("GITHUB_ACTION_PROVIDER_WORK_ROOT")?);
    fs::create_dir_all(&work_root)?;
    let work_metadata = fs::metadata(&work_root)?;
    if !work_metadata.is_dir() || work_metadata.permissions().mode() & 0o077 != 0 {
        anyhow::bail!("GitHub provider work root must be an owner-only directory")
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
        work_root: Arc::new(work_root),
    };
    let app = Router::new()
        .route("/v1/fixed-actions/{operation}", post(execute))
        .route("/v1/fixed-actions/status", get(status))
        .layer(DefaultBodyLimit::max(17 * 1024 * 1024))
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
    let patch = r
        .spec
        .get("patch")
        .and_then(Value::as_str)
        .context("canonical patch missing")?;
    if patch.len() > 16 * 1024 * 1024
        || format!("sha256:{}", hex::encode(Sha256::digest(patch.as_bytes())))
            != r.immutable_input_digest
    {
        anyhow::bail!("canonical patch differs from approved artifact digest")
    }
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
    let commit = create_patched_commit(state, r, repository, branch).await?;
    match r.operation.as_str() {
        "create-branch" => inspect_branch(state, repository, branch, &commit).await,
        "open-pr" => {
            if inspect_branch(state, repository, branch, &commit)
                .await?
                .is_none()
            {
                anyhow::bail!("patched branch is not visible after push")
            }
            let base = r
                .spec
                .get("pullRequestBase")
                .and_then(Value::as_str)
                .context("pullRequestBase missing")?;
            if !git_ref(base) {
                anyhow::bail!("pullRequestBase is invalid")
            }
            let title = r
                .spec
                .get("pullRequestTitle")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= 256)
                .context("pullRequestTitle missing")?;
            let body = r
                .spec
                .get("pullRequestBody")
                .and_then(Value::as_str)
                .unwrap_or("");
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
                if value.pointer("/head/sha").and_then(Value::as_str) != Some(commit.as_str()) {
                    anyhow::bail!("created pull request head differs from patched commit")
                }
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
            inspect_pr(state, repository, branch, base, &commit).await
        }
        _ => anyhow::bail!("unsupported operation"),
    }
}

async fn create_patched_commit(
    state: &AppState,
    request: &ActionRequest,
    repository: &Repository,
    branch: &str,
) -> Result<String> {
    let repository_url = request
        .spec
        .get("repository")
        .and_then(Value::as_str)
        .context("repository missing")?
        .to_string();
    let base_commit = request
        .spec
        .get("baseCommit")
        .and_then(Value::as_str)
        .context("baseCommit missing")?
        .to_string();
    let patch = request
        .spec
        .get("patch")
        .and_then(Value::as_str)
        .context("canonical patch missing")?
        .as_bytes()
        .to_vec();
    let changed_paths = serde_json::from_value::<Vec<String>>(
        request
            .spec
            .get("changedPaths")
            .cloned()
            .context("changedPaths missing")?,
    )?;
    let object_format = match request
        .spec
        .get("repositoryObjectFormat")
        .and_then(Value::as_str)
    {
        Some("sha256") => GitObjectFormat::Sha256,
        Some("sha1") => GitObjectFormat::Sha1,
        _ => anyhow::bail!("repository object format is invalid"),
    };
    let patch_digest = request.immutable_input_digest.clone();
    let policy_digest = request.policy_digest.clone();
    let approval_id = request.approval_id;
    let action_id = request.fixed_action_id;
    let token = state.github_token.as_str().to_string();
    let work_root = state.work_root.as_ref().clone();
    let branch_prefix = state.branch_prefix.as_str().to_string();
    let branch = branch.to_string();
    let owner = repository.owner.clone();
    let repo = repository.repo.clone();
    tokio::task::spawn_blocking(move || {
        let action_root = work_root.join(action_id.to_string());
        if action_root.exists() {
            fs::remove_dir_all(&action_root)?;
        }
        fs::create_dir(&action_root)?;
        fs::set_permissions(&action_root, fs::Permissions::from_mode(0o700))?;
        let home = action_root.join("home");
        let hooks = action_root.join("empty-hooks");
        fs::create_dir(&home)?;
        fs::create_dir(&hooks)?;
        let mirror = action_root.join("source.git");
        let clone = vec![
            "-c".into(),
            format!("core.hooksPath={}", hooks.display()),
            "-c".into(),
            "filter.lfs.smudge=".into(),
            "-c".into(),
            "filter.lfs.required=false".into(),
            "-c".into(),
            "submodule.recurse=false".into(),
            "clone".into(),
            "--mirror".into(),
            "--no-tags".into(),
            repository_url.clone(),
            mirror.display().to_string(),
        ];
        run_git(&clone, &home, &token, &[])?;
        let apply_root = action_root.join("apply");
        let local_repository = mirror.display().to_string();
        let fixed = FixedPatchRequest {
            request_id: action_id,
            repository: local_repository.clone(),
            base_commit: base_commit.clone(),
            repository_object_format: object_format,
            target_branch: branch.clone(),
            patch_artifact_ref: "approved-inline-patch".into(),
            patch_digest,
            policy_digest,
            approval_id,
            changed_paths,
        };
        execute_fixed_patch_in_workspace(
            &fixed,
            &patch,
            &local_repository,
            &branch_prefix,
            &apply_root,
            &ProtectedPathPolicy::default_deny(),
        )?;
        let checkout = apply_root.join("checkout");
        let tree = String::from_utf8(run_git(
            &[
                "-C".into(),
                checkout.display().to_string(),
                "write-tree".into(),
            ],
            &home,
            &token,
            &[],
        )?)?
        .trim()
        .to_string();
        let identity = [
            ("GIT_AUTHOR_NAME", "Light Agent"),
            ("GIT_AUTHOR_EMAIL", "light-agent@users.noreply.github.com"),
            ("GIT_COMMITTER_NAME", "Light Agent"),
            ("GIT_COMMITTER_EMAIL", "light-agent@users.noreply.github.com"),
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
        ];
        let commit = String::from_utf8(run_git(
            &[
                "-C".into(),
                checkout.display().to_string(),
                "commit-tree".into(),
                tree,
                "-p".into(),
                base_commit,
                "-m".into(),
                format!("Approved automated change {action_id}"),
            ],
            &home,
            &token,
            &identity,
        )?)?
        .trim()
        .to_string();
        if !matches!(commit.len(), 40 | 64)
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("git returned an invalid commit object ID")
        }
        // Creation is compare-and-set: an existing branch is never rewritten.
        // A lost response is reconciled against this deterministic commit ID.
        let _ = run_git(
            &[
                "-C".into(),
                checkout.display().to_string(),
                "push".into(),
                "--porcelain".into(),
                format!("--force-with-lease=refs/heads/{branch}:"),
                repository_url,
                format!("{commit}:refs/heads/{branch}"),
            ],
            &home,
            &token,
            &[],
        );
        fs::remove_dir_all(&action_root)?;
        tracing::info!(repository=%format!("{owner}/{repo}"), %branch, %commit, "published approved deterministic commit");
        anyhow::Ok(commit)
    })
    .await?
}

fn run_git(
    arguments: &[String],
    home: &FsPath,
    token: &str,
    additional_environment: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .env_clear()
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        );
    for (name, value) in additional_environment {
        command.env(name, value);
    }
    let output = command.output()?;
    if !output.status.success()
        || output.stdout.len() > 1024 * 1024
        || output.stderr.len() > 1024 * 1024
    {
        anyhow::bail!("trusted git operation failed")
    }
    Ok(output.stdout)
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
    expected_commit: &str,
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
    if value.pointer("/head/sha").and_then(Value::as_str) != Some(expected_commit)
        || value.pointer("/base/ref").and_then(Value::as_str) != Some(base)
    {
        anyhow::bail!("existing pull request differs from approved commit or base")
    }
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
    async fn lost_branch_response_reconciles_then_opens_pr_at_patched_commit() {
        let gets = Arc::new(AtomicUsize::new(0));
        let get_count = gets.clone();
        let posts = Arc::new(AtomicUsize::new(0));
        let post_count = posts.clone();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        fs::create_dir(&source).unwrap();
        test_git(None, &["init", source.to_str().unwrap()]);
        fs::write(source.join("fixture.txt"), "before\n").unwrap();
        test_git(Some(&source), &["add", "fixture.txt"]);
        test_git(
            Some(&source),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "base",
            ],
        );
        let base = test_git_output(Some(&source), &["rev-parse", "HEAD"]);
        fs::write(source.join("fixture.txt"), "after\n").unwrap();
        let patch = String::from_utf8(test_git_bytes(
            Some(&source),
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--no-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ],
        ))
        .unwrap();
        test_git(Some(&source), &["checkout", "--", "fixture.txt"]);
        test_git(
            None,
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let remote_for_branch = remote.clone();
        let remote_for_pr = remote.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/repos/o/r/git/ref/heads/agent/change",
                        get(move || {
                            let count = get_count.clone();
                            let remote = remote_for_branch.clone();
                            async move {
                                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})))
                                        .into_response();
                                }
                                let sha = test_git_output(
                                    Some(&remote),
                                    &["rev-parse", "refs/heads/agent/change"],
                                );
                                (StatusCode::OK, Json(json!({"object":{"sha":sha}})))
                                    .into_response()
                            }
                        }),
                    )
                    .route(
                        "/repos/o/r/pulls",
                        post(move |Json(body): Json<Value>| {
                            let count = post_count.clone();
                            let remote = remote_for_pr.clone();
                            async move {
                                count.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(body["head"], "agent/change");
                                assert_eq!(body["base"], "main");
                                assert_eq!(body["title"], "Approved fixture change");
                                let sha = test_git_output(
                                    Some(&remote),
                                    &["rev-parse", "refs/heads/agent/change"],
                                );
                                (
                                    StatusCode::CREATED,
                                    Json(json!({
                                        "number": 17,
                                        "html_url": "https://github.test/o/r/pull/17",
                                        "head": {"sha": sha},
                                        "base": {"ref": "main"}
                                    })),
                                )
                            }
                        }),
                    ),
            )
            .await
            .unwrap();
        });
        let db = Connection::open(dir.path().join("journal.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE operation_journal(idempotency_key TEXT PRIMARY KEY,operation TEXT NOT NULL,request_json TEXT NOT NULL,state TEXT NOT NULL,receipt_json TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);").unwrap();
        let work_root = dir.path().join("work");
        fs::create_dir(&work_root).unwrap();
        fs::set_permissions(&work_root, fs::Permissions::from_mode(0o700)).unwrap();
        let state = AppState {
            db: Arc::new(Mutex::new(db)),
            http: reqwest::Client::new(),
            api: Url::parse(&format!("http://{address}/")).unwrap(),
            service_secret: Arc::new(vec![b's'; 32]),
            github_token: Arc::new("github-token-123456789".into()),
            repositories: Arc::new(BTreeMap::from([(
                remote.display().to_string(),
                Repository {
                    owner: "o".into(),
                    repo: "r".into(),
                },
            )])),
            branch_prefix: Arc::new("agent/".into()),
            work_root: Arc::new(work_root),
        };
        let patch_digest = format!("sha256:{}", hex::encode(Sha256::digest(patch.as_bytes())));
        let request = ActionRequest {
            fixed_action_id: uuid::Uuid::now_v7(),
            execution_id: uuid::Uuid::now_v7(),
            approval_id: uuid::Uuid::now_v7(),
            operation: "open-pr".into(),
            immutable_input_digest: patch_digest.clone(),
            target: json!(remote.display().to_string()),
            policy_digest: "sha256:policy".into(),
            provenance_digest: Some("sha256:provenance".into()),
            spec: json!({"operation":"open-pr","target":remote.display().to_string(),"repository":remote.display().to_string(),"baseCommit":base,"repositoryObjectFormat":"sha1","targetBranch":"agent/change","patchDigest":patch_digest,"patch":patch,"changedPaths":["fixture.txt"],"pullRequestBase":"main","pullRequestTitle":"Approved fixture change","policyDigest":"sha256:policy","provenanceDigest":"sha256:provenance"}),
        };
        assert!(
            execute_inner(
                &state,
                "approval:fixed-action-0001",
                "open-pr",
                request.clone(),
            )
            .await
            .is_err()
        );
        let replay = execute_inner(
            &state,
            "approval:fixed-action-0001",
            "open-pr",
            request.clone(),
        )
        .await
        .unwrap();
        let mut substituted = request;
        substituted.spec["targetBranch"] = json!("agent/substituted");
        assert!(
            execute_inner(&state, "approval:fixed-action-0001", "open-pr", substituted)
                .await
                .is_err()
        );
        let branch_commit =
            test_git_output(Some(&remote), &["rev-parse", "refs/heads/agent/change"]);
        assert_eq!(replay.provider_operation_id, "pr:o/r#17");
        assert_ne!(branch_commit, base);
        assert_eq!(gets.load(Ordering::SeqCst), 2);
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }

    fn test_git(workspace: Option<&FsPath>, arguments: &[&str]) {
        let output = test_git_command(workspace, arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn test_git_output(workspace: Option<&FsPath>, arguments: &[&str]) -> String {
        String::from_utf8(test_git_bytes(workspace, arguments))
            .unwrap()
            .trim()
            .into()
    }
    fn test_git_bytes(workspace: Option<&FsPath>, arguments: &[&str]) -> Vec<u8> {
        let output = test_git_command(workspace, arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    fn test_git_command(workspace: Option<&FsPath>, arguments: &[&str]) -> Command {
        let mut command = Command::new("git");
        if let Some(workspace) = workspace {
            command.arg("-C").arg(workspace);
        }
        command.args(arguments);
        command
    }
}
