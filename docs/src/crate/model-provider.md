# Model Provider

`model-provider` defines a common abstraction over LLM providers and implements
multiple provider adapters.

The goal is to let agent and workflow code depend on one `Provider` trait while
supporting local models, hosted APIs, and provider-specific features.

## Main Types

- `Provider`: async trait implemented by model providers.
- `ChatRequest`, `ChatResponse`, `ChatMessage`: common chat data model.
- `ToolSpec`, `ToolCall`: tool-calling model.
- `ProviderCapabilities`: capability metadata.
- `TokenUsage`: usage accounting.
- `ReliableProvider`: reliability wrapper.
- `RouterProvider`: route requests across multiple providers.

## Provider Implementations

Current modules include:

- Anthropic
- Azure OpenAI
- Bedrock
- Claude Code
- Codex
- OpenAI-compatible providers
- Copilot
- Gemini
- Gemini CLI
- GLM
- Kilo Code CLI
- Ollama
- OpenAI
- OpenRouter
- Telnyx

## Consumers

`light-agent` uses this crate to send chat requests and tool specs without
hard-coding a single LLM provider.
