# Model Providers

Light-RS provides a unified interface for multiple LLM providers. All providers implement the `Provider` trait.

## Supported Providers

- **OpenAI**: Support for GPT-4o, GPT-3.5-turbo, and Reasoning models.
- **Anthropic**: Support for Claude 3.5 Sonnet/Haiku/Opus.
- **Gemini**: Support for Google's Gemini Pro and Flash.
- **OpenRouter**: Access to hundreds of models via a single API.
- **Ollama**: Local LLM support.
- **Azure OpenAI**: Enterprise-grade OpenAI deployments.
- **AWS Bedrock**: Support for Claude and Titan on AWS.
- **GLM**: Support for Zhipu AI models.

## Meta-Providers

- **ReliableProvider**: Adds retries, backoff, and failover.
- **RouterProvider**: Logic-based routing using hints.

## CLI Providers

- **Claude Code CLI**
- **Gemini CLI**
- **KiloCLI**
