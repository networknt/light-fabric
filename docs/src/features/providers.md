# Model Providers

Light-Fabric provides a unified, high-performance interface for interacting with diverse Large Language Model (LLM) providers. This abstraction is centered around the `Provider` trait, allowing applications to remain model-agnostic while leveraging advanced capabilities like native tool calling and prompt caching.

## The Provider Trait

All model integrations implement the `Provider` trait, which supports:
- **One-shot and Multi-turn Chat**: Simplified APIs for simple prompts and full conversation histories.
- **Structured Tool Calling**: Native integration for function calling (OpenAI-style).
- **Capabilities Detection**: Programmatic checks for vision, native tool support, and prompt caching.

## Supported Cloud Providers

Light-Fabric supports all major LLM providers. Because the `Provider` trait is model-agnostic, the framework is compatible with the latest flagship releases as soon as they are available.

- **OpenAI**: Native support for the **GPT-5 series (5.4, mini, nano)**, the **o4 reasoning models**, and full legacy support for GPT-4o and GPT-4 Turbo.
- **Anthropic**: Support for the **Claude 4** generation, including **Opus 4.7**, Sonnet, and Haiku.
- **Google Gemini**: Support for **Gemini 3.1 Pro and Flash**, leveraging Vertex AI or AI Studio for multi-modal and long-context tasks.
- **Azure OpenAI**: Enterprise-grade OpenAI deployments with support for the latest model deployments.
- **AWS Bedrock**: Access to the latest Claude and Titan models hosted on Amazon Web Services.
- **OpenRouter**: Access to hundreds of open-source and proprietary models via a single unified API.
- **Telnyx**: Support for models hosted on the Telnyx platform.
- **GLM (Zhipu AI)**: Support for the ChatGLM/GLM-5 series of models.

## Local & Specialized Providers

- **Ollama**: Seamless integration with local models running on your machine.
- **OpenAI-Compatible**: A generic `CompatibleProvider` for any service implementing the OpenAI REST API.
- **GitHub Copilot**: Integration with GitHub Copilot Chat for developer-centric workflows.

## Meta-Providers (Orchestration)

These providers wrap other providers to add resilient or intelligent behavior:

- **ReliableProvider**: Enhances any base provider with retries, exponential backoff, and automatic failover to fallback models.
- **RouterProvider**: Dynamically routes requests to different models based on hints or input complexity.

## CLI & Tooling Integrations

Light-Fabric includes specialized integrations for developer tools and terminal environments:

- **Claude Code CLI**: Integration with Anthropic's Claude Code environment.
- **Gemini CLI**: Terminal-based access to Google's Gemini models.
- **KiloCLI**: Light-Fabric's native CLI integration for rapid testing and automation.

## Key Capabilities

Providers can be queried for their support of advanced features:
- **Native Tool Calling**: Efficiently generate structured function calls.
- **Vision**: Process images alongside text prompts.
- **Prompt Caching**: Leverage provider-side caching to reduce latency and costs for long contexts.
