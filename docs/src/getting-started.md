# Getting Started

To get started with Light-RS, add the necessary crates to your `Cargo.toml`.

## Installation

```toml
[dependencies]
light-model-provider = { git = "https://github.com/networknt/light-rs" }
```

## Basic Usage

Here is a quick example of how to initialize an OpenAI provider:

```rust
use light_model_provider::{OpenAiProvider, Provider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = OpenAiProvider::new(Some("your-api-key"))?;
    let response = provider.chat_with_system(
        Some("You are a helpful assistant."),
        "Hello!",
        "gpt-4o",
        0.7
    ).await?;
    
    println!("Response: {}", response);
    Ok(())
}
```
