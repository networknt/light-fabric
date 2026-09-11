use crate::{FileEdit, WorkspaceToolSession};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase", deny_unknown_fields)]
enum ToolRequest {
    Repositories,
    Files {
        repository: String,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        offset: usize,
    },
    Read {
        repository: String,
        path: String,
    },
    Edit {
        repository: String,
        path: String,
        content: Option<String>,
        #[serde(rename = "expectedDigest")]
        expected_digest: Option<String>,
    },
}

/// This schema deliberately has no identity, task selector, host path or command.
pub fn workspace_tool_definition() -> Value {
    json!({"name":"task_workspace", "description":"Access the admitted task. List repositories, list files (500 per page), read UTF-8 files, or atomically edit with the digest returned by read. Edit content=null deletes; expectedDigest=null creates a new file. No shell or Git operations.",
        "inputSchema":{"type":"object","properties":{
            "operation":{"type":"string","enum":["repositories","files","read","edit"]},
            "repository":{"type":"string"},"prefix":{"type":"string"},"offset":{"type":"integer","minimum":0},
            "path":{"type":"string"},"content":{"type":["string","null"]},"expectedDigest":{"type":["string","null"]}
        },"required":["operation"],"additionalProperties":false}})
}

impl WorkspaceToolSession {
    pub fn call_tool(&mut self, arguments: Value) -> Result<Value> {
        ensure!(
            serde_json::to_vec(&arguments)?.len() <= 1100 * 1024,
            "tool request exceeds limit"
        );
        if arguments.get("operation").and_then(Value::as_str) == Some("edit") {
            ensure!(
                arguments.get("content").is_some() && arguments.get("expectedDigest").is_some(),
                "edit requires explicit content and expectedDigest, including null for deletion or creation"
            );
        }
        let request: ToolRequest = serde_json::from_value(arguments)?;
        match request {
            ToolRequest::Repositories => Ok(
                json!({"repositories":self.checkpoint().repositories.iter().map(|r| &r.repository).collect::<Vec<_>>() }),
            ),
            ToolRequest::Files {
                repository,
                prefix,
                offset,
            } => {
                let files: Vec<_> = self
                    .checkpoint()
                    .repositories
                    .iter()
                    .find(|r| r.repository == repository)
                    .ok_or_else(|| anyhow::anyhow!("repository is not in this task"))?
                    .files
                    .iter()
                    .filter(|f| f.path.starts_with(&prefix))
                    .collect();
                Ok(
                    json!({"files":files.iter().skip(offset).take(500).collect::<Vec<_>>(),
                    "nextOffset": offset.checked_add(500).filter(|next| *next < files.len())}),
                )
            }
            ToolRequest::Read { repository, path } => {
                Ok(serde_json::to_value(self.read_file(&repository, &path)?)?)
            }
            ToolRequest::Edit {
                repository,
                path,
                content,
                expected_digest,
            } => Ok(
                json!({"checkpointDigest":self.edit_file(FileEdit { repository, path, content, expected_digest })?}),
            ),
        }
    }
}
