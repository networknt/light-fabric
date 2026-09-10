use crate::{Repository, Workspace, git};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRepository {
    pub directory: PathBuf,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub skipped_repositories: Vec<SkippedRepository>,
    pub workspace: Workspace,
    /// Local refs only; remote readiness must be verified during task creation.
    pub integration_branch_not_known_locally: Vec<String>,
}

pub fn discover(
    root: &Path,
    id: &str,
    host_id: &str,
    agents: BTreeSet<String>,
) -> Result<Discovery> {
    git::valid_id(id)?;
    let root = root.canonicalize()?;
    let mut entries = fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut repositories = Vec::new();
    let mut missing = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        let path = entry.path();
        if !entry.file_type()?.is_dir() || !path.join(".git").exists() {
            continue;
        }
        let candidate = (|| -> Result<(String, String)> {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("repository name is not UTF-8"))?;
            git::valid_id(&name)?;
            let source = git::text(&path, &["remote", "get-url", "origin"])
                .map_err(|_| anyhow::anyhow!("repository origin could not be read"))?;
            Ok((name, source))
        })();
        let (name, source) = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                skipped.push(SkippedRepository {
                    directory: path,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if git::text(&path, &["rev-parse", "--verify", "refs/heads/develop"]).is_err()
            && git::text(
                &path,
                &["rev-parse", "--verify", "refs/remotes/origin/develop"],
            )
            .is_err()
        {
            missing.push(name.clone());
        }
        repositories.push(Repository {
            name,
            source,
            integration_branch: "develop".into(),
            release_branch: "master".into(),
        });
    }
    ensure!(
        !repositories.is_empty() || !skipped.is_empty(),
        "no Git repositories found"
    );
    Ok(Discovery {
        skipped_repositories: skipped,
        workspace: Workspace {
            schema_version: 1,
            id: id.into(),
            host_id: host_id.into(),
            agents,
            repositories,
            operations: BTreeSet::new(),
            indexers: BTreeMap::new(),
        },
        integration_branch_not_known_locally: missing,
    })
}
