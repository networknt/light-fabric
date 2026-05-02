use crate::model::TemplateRef;
use crate::renderer::TemplateDocument;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TemplateSourceError {
    #[error("failed to read template path `{path}`: {message}")]
    Read { path: String, message: String },
    #[error("failed to clone template repository: {0}")]
    Clone(String),
    #[error("template path is invalid: {0}")]
    InvalidPath(String),
    #[error("template repository produced no YAML documents at `{0}`")]
    Empty(String),
    #[error("local template base directory is required when template.repoUrl is `local`")]
    MissingLocalBaseDir,
    #[error("local template path does not exist: {0}")]
    MissingLocalPath(String),
}

#[derive(Debug, Clone)]
pub struct TemplateBundle {
    pub commit_sha: Option<String>,
    pub documents: Vec<TemplateDocument>,
}

#[async_trait::async_trait]
pub trait TemplateSource: Send + Sync {
    async fn fetch(&self, template: &TemplateRef) -> Result<TemplateBundle, TemplateSourceError>;
}

#[derive(Debug, Clone, Default)]
pub struct LocalTemplateSource {
    pub base_dir: Option<PathBuf>,
    pub remote_cache_dir: Option<PathBuf>,
}

#[async_trait::async_trait]
impl TemplateSource for LocalTemplateSource {
    async fn fetch(&self, template: &TemplateRef) -> Result<TemplateBundle, TemplateSourceError> {
        let is_local_template = template.repo_url.eq_ignore_ascii_case("local");
        if let Some(base_dir) = &self.base_dir {
            let path = base_dir.join(&template.path);
            if path.exists() {
                let documents = read_template_documents(&path).await?;
                return Ok(TemplateBundle {
                    commit_sha: None,
                    documents,
                });
            }
            if is_local_template {
                return Err(TemplateSourceError::MissingLocalPath(
                    path.display().to_string(),
                ));
            }
        } else if is_local_template {
            return Err(TemplateSourceError::MissingLocalBaseDir);
        }

        let template = template.clone();
        let remote_cache_dir = self.remote_cache_dir.clone();
        tokio::task::spawn_blocking(move || clone_and_read(template, remote_cache_dir))
            .await
            .map_err(|error| TemplateSourceError::Clone(error.to_string()))?
    }
}

async fn read_template_documents(
    path: &Path,
) -> Result<Vec<TemplateDocument>, TemplateSourceError> {
    validate_template_path(path)?;
    let mut documents = Vec::new();
    let mut stack = vec![path.to_path_buf()];

    while let Some(path) = stack.pop() {
        let mut entries =
            tokio::fs::read_dir(&path)
                .await
                .map_err(|error| TemplateSourceError::Read {
                    path: path.display().to_string(),
                    message: error.to_string(),
                })?;
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(|error| TemplateSourceError::Read {
                    path: path.display().to_string(),
                    message: error.to_string(),
                })?
        {
            let entry_path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| TemplateSourceError::Read {
                    path: entry_path.display().to_string(),
                    message: error.to_string(),
                })?;
            if file_type.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !(file_name.ends_with(".yaml") || file_name.ends_with(".yml")) {
                continue;
            }
            let content = tokio::fs::read_to_string(&entry_path)
                .await
                .map_err(|error| TemplateSourceError::Read {
                    path: entry_path.display().to_string(),
                    message: error.to_string(),
                })?;
            documents.push(TemplateDocument {
                name: entry_path.display().to_string(),
                content,
            });
        }
    }

    documents.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(documents)
}

fn clone_and_read(
    template: TemplateRef,
    remote_cache_dir: Option<PathBuf>,
) -> Result<TemplateBundle, TemplateSourceError> {
    let clone_root = remote_cache_dir.unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&clone_root).map_err(|error| TemplateSourceError::Read {
        path: clone_root.display().to_string(),
        message: error.to_string(),
    })?;
    let checkout_dir = clone_root.join(format!("light-deployer-{}", Uuid::new_v4()));
    let clone_url = authenticated_url(&template.repo_url);

    let result =
        clone_repository(&clone_url, &checkout_dir, &template.r#ref).and_then(|commit_sha| {
            read_template_documents_blocking(&checkout_dir.join(&template.path), commit_sha)
        });

    let _ = std::fs::remove_dir_all(&checkout_dir);
    result
}

fn clone_repository(
    clone_url: &str,
    checkout_dir: &Path,
    ref_name: &str,
) -> Result<Option<String>, TemplateSourceError> {
    let should_interrupt = AtomicBool::new(false);
    let mut prepare = gix::prepare_clone(clone_url, checkout_dir)
        .map_err(|error| TemplateSourceError::Clone(error.to_string()))?;
    if !ref_name.trim().is_empty() {
        prepare = prepare
            .with_ref_name(Some(ref_name))
            .map_err(|error| TemplateSourceError::Clone(format!("{error:?}")))?;
    }
    let (mut checkout, _) = prepare
        .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
        .map_err(|error| TemplateSourceError::Clone(error.to_string()))?;
    let (repo, _) = checkout
        .main_worktree(gix::progress::Discard, &should_interrupt)
        .map_err(|error| TemplateSourceError::Clone(error.to_string()))?;
    let commit_sha = repo.head_id().ok().map(|id| id.to_string());
    Ok(commit_sha)
}

fn read_template_documents_blocking(
    path: &Path,
    commit_sha: Option<String>,
) -> Result<TemplateBundle, TemplateSourceError> {
    validate_template_path(path)?;
    let mut documents = Vec::new();
    let mut stack = vec![path.to_path_buf()];

    while let Some(path) = stack.pop() {
        let entries = std::fs::read_dir(&path).map_err(|error| TemplateSourceError::Read {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| TemplateSourceError::Read {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;
            let entry_path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| TemplateSourceError::Read {
                    path: entry_path.display().to_string(),
                    message: error.to_string(),
                })?;
            if file_type.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !(file_name.ends_with(".yaml") || file_name.ends_with(".yml")) {
                continue;
            }
            let content = std::fs::read_to_string(&entry_path).map_err(|error| {
                TemplateSourceError::Read {
                    path: entry_path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
            documents.push(TemplateDocument {
                name: entry_path.display().to_string(),
                content,
            });
        }
    }

    documents.sort_by(|left, right| left.name.cmp(&right.name));
    if documents.is_empty() {
        return Err(TemplateSourceError::Empty(path.display().to_string()));
    }
    Ok(TemplateBundle {
        commit_sha,
        documents,
    })
}

fn validate_template_path(path: &Path) -> Result<(), TemplateSourceError> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(TemplateSourceError::InvalidPath(path.display().to_string()));
    }
    Ok(())
}

fn authenticated_url(repo_url: &str) -> String {
    let Ok(token) = std::env::var("LIGHT_DEPLOYER_GIT_TOKEN") else {
        return repo_url.to_string();
    };
    let username = std::env::var("LIGHT_DEPLOYER_GIT_USERNAME").ok();
    authenticated_url_with_token(repo_url, token.trim(), username.as_deref())
}

fn authenticated_url_with_token(repo_url: &str, token: &str, username: Option<&str>) -> String {
    if token.is_empty() || !repo_url.starts_with("https://") || repo_url.contains('@') {
        return repo_url.to_string();
    }
    let username = username
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_git_username(repo_url));
    let username = percent_encode_userinfo(username);
    let token = percent_encode_userinfo(token);
    format!(
        "https://{username}:{token}@{}",
        &repo_url["https://".len()..]
    )
}

fn default_git_username(repo_url: &str) -> &'static str {
    if repo_url.contains("://bitbucket.org/") {
        "x-token-auth"
    } else {
        "x-access-token"
    }
}

fn percent_encode_userinfo(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        LocalTemplateSource, TemplateSource, TemplateSourceError, authenticated_url_with_token,
    };
    use crate::model::TemplateRef;

    #[test]
    fn injects_github_token_with_default_username() {
        let url = authenticated_url_with_token(
            "https://github.com/networknt/openapi-petstore.git",
            "ghp_token",
            None,
        );
        assert_eq!(
            url,
            "https://x-access-token:ghp_token@github.com/networknt/openapi-petstore.git"
        );
    }

    #[test]
    fn injects_bitbucket_token_with_default_username() {
        let url =
            authenticated_url_with_token("https://bitbucket.org/acme/petstore.git", "token", None);
        assert_eq!(
            url,
            "https://x-token-auth:token@bitbucket.org/acme/petstore.git"
        );
    }

    #[test]
    fn supports_custom_git_username() {
        let url = authenticated_url_with_token(
            "https://bitbucket.org/acme/petstore.git",
            "app password",
            Some("workspace-user"),
        );
        assert_eq!(
            url,
            "https://workspace-user:app%20password@bitbucket.org/acme/petstore.git"
        );
    }

    #[tokio::test]
    async fn local_marker_requires_base_dir() {
        let source = LocalTemplateSource::default();
        let error = source
            .fetch(&TemplateRef {
                repo_url: "local".to_string(),
                r#ref: "main".to_string(),
                path: "k8s/light-gateway".to_string(),
            })
            .await
            .unwrap_err();

        assert!(matches!(error, TemplateSourceError::MissingLocalBaseDir));
    }
}
