use crate::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, TokenUsage,
};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const GEMINI_CLI_PATH_ENV: &str = "GEMINI_CLI_PATH";
const DEFAULT_GEMINI_CLI_BINARY: &str = "gemini";
const DEFAULT_MODEL_MARKER: &str = "default";
const GEMINI_CLI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_GEMINI_CLI_STDERR_CHARS: usize = 512;

pub struct GeminiCliProvider {
    binary_path: PathBuf,
}

impl GeminiCliProvider {
    pub fn new() -> Self {
        let binary_path = std::env::var(GEMINI_CLI_PATH_ENV)
            .ok()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_GEMINI_CLI_BINARY));

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
        if trimmed.chars().count() <= MAX_GEMINI_CLI_STDERR_CHARS {
            return trimmed.to_string();
        }
        let clipped: String = trimmed.chars().take(MAX_GEMINI_CLI_STDERR_CHARS).collect();
        format!("{clipped}...")
    }

    async fn invoke_cli(&self, message: &str, model: &str) -> anyhow::Result<String> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--print");

        if Self::should_forward_model(model) {
            cmd.arg("--model").arg(model);
        }

        cmd.arg("-");
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|err| {
            anyhow::anyhow!(
                "Failed to spawn Gemini CLI binary at {}: {err}",
                self.binary_path.display()
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = timeout(GEMINI_CLI_REQUEST_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("Gemini CLI request timed out"))??;

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            let stderr_excerpt = Self::redact_stderr(&output.stderr);
            anyhow::bail!("Gemini CLI exited with status {code}. Stderr: {stderr_excerpt}");
        }

        let text = String::from_utf8(output.stdout)?;
        Ok(text.trim().to_string())
    }
}

impl Default for GeminiCliProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GeminiCliProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let full_message = match system_prompt {
            Some(system) if !system.is_empty() => format!("{system}\n\n{message}"),
            _ => message.to_string(),
        };
        self.invoke_cli(&full_message, model).await
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
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        self.chat_with_system(system, last_user, model, temperature)
            .await
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
            usage: Some(TokenUsage::default()),
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
