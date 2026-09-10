use crate::{
    Operation, TaskState, WorkspaceStore, checkpoint, git,
    store::{lock, read, write},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{Seek, Write},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryReceipt {
    pub operation: String,
    pub input_digest: String,
    pub state: String,
    pub resource: Option<String>,
}

fn github_repository(source: &str) -> Result<String> {
    let repository = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("git@github.com:"))
        .context("GitHub action requires a github.com clone source")?
        .trim_end_matches(".git");
    let parts: Vec<_> = repository.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|part| !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
        "invalid GitHub repository"
    );
    Ok(repository.into())
}
fn gh(args: &[&str], body: Option<&str>) -> Result<String> {
    let mut input = tempfile::tempfile()?;
    if let Some(body) = body {
        input.write_all(body.as_bytes())?;
    }
    input.rewind()?;
    let mut command = Command::new("gh");
    command
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::from(input));
    let output = crate::store::bounded_command(&mut command, 120)?;
    ensure!(
        !output.timed_out && output.exit_code == Some(0),
        "GitHub action failed; reconcile the recorded operation before retrying"
    );
    Ok(output.stdout.trim().into())
}

impl WorkspaceStore {
    /// No force push. A remote task ref must be absent or already equal to the
    /// reviewed local commit. This action never pushes develop or master.
    pub fn push(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
    ) -> Result<Vec<DeliveryReceipt>> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Push),
            "push operation is not granted"
        );
        let _workspace_lock = lock(&self.workspace_path(&workspace.id)?.join("workspace.lock"))?;
        let root = self.task_path(&workspace.id, task_id)?;
        let _task_lock = lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            task.state == TaskState::Committed,
            "only reviewed commits can be pushed"
        );
        let mut receipts = Vec::new();
        for checkout in &task.checkouts {
            checkpoint::validate_checkout(checkout)?;
            let commit = checkout.commit.as_ref().context("commit receipt missing")?;
            if commit == &checkout.base_commit {
                continue;
            }
            ensure!(
                git::text(&checkout.path, &["rev-parse", "HEAD"])? == *commit
                    && git::run(
                        &checkout.path,
                        &["status", "--porcelain=v1", "-z", "--untracked-files=all"]
                    )?
                    .is_empty(),
                "local commit changed after review"
            );
            let branch = format!("refs/heads/{}", checkout.branch);
            let remote = git::text(&checkout.path, &["ls-remote", "--refs", "origin", &branch])?;
            let remote_commit = remote.split_whitespace().next();
            ensure!(
                remote_commit.is_none() || remote_commit == Some(commit.as_str()),
                "remote task branch contains unexpected changes"
            );
            let path = root.join(format!("push-{}.json", checkout.repository));
            let input_digest = checkpoint::digest(&serde_json::to_vec(
                &json!({"repository":checkout.repository,"commit":commit,"branch":branch}),
            )?);
            let mut receipt = DeliveryReceipt {
                operation: "push".into(),
                input_digest,
                state: "in-flight".into(),
                resource: None,
            };
            if path.exists() {
                let previous: DeliveryReceipt = read(&path)?;
                ensure!(
                    previous.input_digest == receipt.input_digest,
                    "push input differs from recorded action"
                );
            }
            write(&path, &receipt)?;
            if remote_commit.is_none() {
                // Lease expects absence even if another actor creates the ref
                // between ls-remote and push; do not overwrite that actor.
                git::run(
                    &checkout.path,
                    &[
                        "push",
                        &format!("--force-with-lease={branch}:"),
                        "origin",
                        &format!("{commit}:{branch}"),
                    ],
                )?;
            }
            receipt.state = "succeeded".into();
            receipt.resource = Some(format!("{}@{}", checkout.branch, commit));
            write(&path, &receipt)?;
            receipts.push(receipt);
        }
        Ok(receipts)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn github(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        repository: &str,
        action: &str,
        title: &str,
        body: &str,
    ) -> Result<DeliveryReceipt> {
        ensure!(
            !title.trim().is_empty() && title.len() <= 256 && body.len() <= 64 * 1024,
            "invalid GitHub title or body"
        );
        let workspace = self.workspace(workspace, agent)?;
        let permission = match action {
            "issue" => Operation::Issue,
            "pull-request" => Operation::PullRequest,
            _ => anyhow::bail!("unknown GitHub action"),
        };
        ensure!(
            workspace.operations.contains(&permission),
            "GitHub operation is not granted"
        );
        let root = self.task_path(&workspace.id, task_id)?;
        let _lock = lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        let repo = workspace
            .repositories
            .iter()
            .find(|r| r.name == repository)
            .context("repository not in workspace")?;
        let github_repo = github_repository(&repo.source)?;
        let checkout = task
            .checkouts
            .iter()
            .find(|r| r.repository == repository)
            .context("task checkout missing")?;
        if action == "pull-request" {
            ensure!(
                task.state == TaskState::Committed,
                "PR requires reviewed commits"
            );
            let push: DeliveryReceipt = read(&root.join(format!("push-{repository}.json")))
                .context("push receipt is required before PR")?;
            ensure!(push.state == "succeeded", "push is incomplete");
            checkpoint::validate_checkout(checkout)?;
            let expected = checkout
                .commit
                .as_ref()
                .context("reviewed commit missing")?;
            let remote = git::text(
                &checkout.path,
                &[
                    "ls-remote",
                    "--refs",
                    "origin",
                    &format!("refs/heads/{}", checkout.branch),
                ],
            )?;
            ensure!(
                remote.split_whitespace().next() == Some(expected.as_str()),
                "remote PR head changed after reviewed push"
            );
        }
        let key = checkpoint::digest(
            format!("{}:{}:{}:{action}", workspace.id, task_id, repository).as_bytes(),
        );
        let marker = format!("<!-- light-workspace {} -->", key);
        let body = format!("{body}\n\n{marker}");
        let input_digest = checkpoint::digest(&serde_json::to_vec(
            &json!({"repo":github_repo,"action":action,"title":title,"body":body,"branch":checkout.branch,"base":checkout.integration_branch}),
        )?);
        let path = root.join(format!("{action}-{repository}.json"));
        if path.exists() {
            let mut prior: DeliveryReceipt = read(&path)?;
            ensure!(
                prior.input_digest == input_digest,
                "GitHub retry differs from recorded action"
            );
            if prior.state == "succeeded" {
                return Ok(prior);
            }
            // Enumerate bodies directly: search does not reliably index hidden markers.
            let values: Vec<Value> = if action == "issue" {
                let mut values = Vec::new();
                let mut complete = false;
                for page in 1..=10000 {
                    let endpoint = format!(
                        "repos/{github_repo}/issues?state=all&sort=created&direction=asc&per_page=100&page={page}"
                    );
                    let output = gh(&["api", &endpoint], None)?;
                    let batch: Vec<Value> = serde_json::from_str(&output)?;
                    let last = batch.len() < 100;
                    values.extend(
                        batch
                            .into_iter()
                            .filter(|v| {
                                v.get("pull_request").is_none()
                                    && v["body"]
                                        .as_str()
                                        .is_some_and(|body| body.contains(&marker))
                            })
                            .map(|mut v| {
                                v["url"] = v["html_url"].clone();
                                v
                            }),
                    );
                    if last {
                        complete = true;
                        break;
                    }
                }
                ensure!(
                    complete,
                    "issue enumeration incomplete; no duplicate create attempted"
                );
                values
            } else {
                let output = gh(
                    &[
                        "pr",
                        "list",
                        "--repo",
                        &github_repo,
                        "--state",
                        "all",
                        "--head",
                        &checkout.branch,
                        "--base",
                        &checkout.integration_branch,
                        "--json",
                        "url,body,headRefOid,baseRefName",
                    ],
                    None,
                )?;
                serde_json::from_str(&output)?
            };
            let found: Vec<_> = values
                .iter()
                .filter(|v| v["body"].as_str().is_some_and(|b| b.contains(&marker)))
                .collect();
            ensure!(
                found.len() == 1,
                "uncertain GitHub action requires reconciliation; no duplicate create attempted"
            );
            if action == "pull-request" {
                ensure!(
                    found[0]["headRefOid"].as_str() == checkout.commit.as_deref()
                        && found[0]["baseRefName"].as_str()
                            == Some(checkout.integration_branch.as_str()),
                    "existing PR differs from reviewed head or integration branch"
                );
            }
            prior.resource = Some(
                found[0]["url"]
                    .as_str()
                    .context("GitHub resource URL missing")?
                    .into(),
            );
            prior.state = "succeeded".into();
            write(&path, &prior)?;
            return Ok(prior);
        }
        let mut receipt = DeliveryReceipt {
            operation: action.into(),
            input_digest,
            state: "in-flight".into(),
            resource: None,
        };
        write(&path, &receipt)?;
        let resource = if action == "issue" {
            gh(
                &[
                    "issue",
                    "create",
                    "--repo",
                    &github_repo,
                    "--title",
                    title,
                    "--body-file",
                    "-",
                ],
                Some(&body),
            )?
        } else {
            gh(
                &[
                    "pr",
                    "create",
                    "--repo",
                    &github_repo,
                    "--head",
                    &checkout.branch,
                    "--base",
                    &checkout.integration_branch,
                    "--title",
                    title,
                    "--body-file",
                    "-",
                ],
                Some(&body),
            )?
        };
        receipt.resource = Some(resource);
        if action == "pull-request" {
            let inspected: Value = serde_json::from_str(&gh(
                &[
                    "pr",
                    "view",
                    receipt.resource.as_deref().context("PR URL missing")?,
                    "--repo",
                    &github_repo,
                    "--json",
                    "headRefOid,baseRefName",
                ],
                None,
            )?)?;
            ensure!(
                inspected["headRefOid"].as_str() == checkout.commit.as_deref()
                    && inspected["baseRefName"].as_str()
                        == Some(checkout.integration_branch.as_str()),
                "created PR differs from reviewed head or integration branch"
            );
        }
        receipt.state = "succeeded".into();
        write(&path, &receipt)?;
        Ok(receipt)
    }
}
