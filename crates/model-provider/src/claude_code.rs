use crate::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, TokenUsage,
};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const CLAUDE_CODE_PATH_ENV: &str = "CLAUDE_CODE_PATH";
const DEFAULT_CLAUDE_CODE_BINARY: &str = "claude";
const DEFAULT_MODEL_MARKER: &str = "default";
const CLAUDE_CODE_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CLAUDE_CODE_STDERR_CHARS: usize = 512;

pub struct ClaudeCodeProvider {
    binary_path: PathBuf,
}

impl ClaudeCodeProvider {
    pub fn new() -> Self {
        let binary_path = std::env::var(CLAUDE_CODE_PATH_ENV)
            .ok()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CLAUDE_CODE_BINARY));

        Self { binary_path }
    }

    fn should_forward_model(model: &str) -> bool {
        let trimmed = model.trim();
        !trimmed.is_empty() && trimmed != DEFAULT_MODEL_MARKER
    }

    fn redact_stderr(stderr: &[u8]) -> String {
        let text = String::from_utf8_lossy(stderr);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        if trimmed.chars().count() <= MAX_CLAUDE_CODE_STDERR_CHARS {
            return trimmed.to_string();
        }
        let clipped: String = trimmed.chars().take(MAX_CLAUDE_CODE_STDERR_CHARS).collect();
        format!("{clipped}...")
    }

    async fn invoke_cli(
        &self,
        message: &str,
        model: &str,
        system_prompt: Option<&str>,
        agent_mode: bool,
    ) -> anyhow::Result<(String, Option<TokenUsage>)> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--print");

        if agent_mode {
            cmd.arg("--dangerously-skip-permissions");
            cmd.arg("--output-format").arg("json");
        }

        if Self::should_forward_model(model) {
            cmd.arg("--model").arg(model);
        }

        if let Some(sp) = system_prompt
            && !sp.is_empty()
        {
            cmd.arg("--append-system-prompt").arg(sp);
        }

        cmd.arg("-");
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|err| {
            anyhow::anyhow!(
                "Failed to spawn Claude Code binary at {}: {err}",
                self.binary_path.display()
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = timeout(CLAUDE_CODE_REQUEST_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("Claude Code request timed out"))??;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let stderr_excerpt = Self::redact_stderr(&output.stderr);
            anyhow::bail!("Claude Code exited with status {code}. Stderr: {stderr_excerpt}");
        }

        let raw = String::from_utf8(output.stdout)?;

        if agent_mode && let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
            let text = json
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            let usage = json.get("usage").map(|u| TokenUsage {
                input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()),
                output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
                cached_input_tokens: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            });

            return Ok((text, usage));
        }

        Ok((raw.trim().to_string(), None))
    }
}

impl Default for ClaudeCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: false,
            prompt_caching: true,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let (text, _usage) = self.invoke_cli(message, model, system_prompt, true).await?;
        Ok(text)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.as_str());
        let turns: Vec<&ChatMessage> = messages.iter().filter(|m| m.role != "system").collect();

        let user_message = if turns.len() <= 1 {
            turns.first().map(|m| m.content.clone()).unwrap_or_default()
        } else {
            let mut parts = Vec::new();
            for msg in &turns {
                let label = match msg.role.as_str() {
                    "user" => "[user]",
                    "assistant" => "[assistant]",
                    other => other,
                };
                parts.push(format!("{label}\n{}", msg.content));
            }
            parts.push("[assistant]".to_string());
            parts.join("\n\n")
        };

        let (text, _usage) = self.invoke_cli(&user_message, model, system, true).await?;
        Ok(text)
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let text = self
            .chat_with_history(request.messages, model, temperature)
            .await?;
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        })
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.chat(
            ChatRequest {
                messages,
                tools: None,
            },
            model,
            temperature,
        )
        .await
    }
}
