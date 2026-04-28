# Hindsight Client

`hindsight-client` provides a small client abstraction for persistent agent
memory.

It stores and recalls memory units from PostgreSQL. The current implementation
uses `sqlx` and `pgvector` for vector similarity search.

## Main Types

- `HindsightMemory`: trait used by applications that need memory retention and
  recall without coupling to a specific database implementation.
- `PgHindsightClient`: PostgreSQL-backed implementation of `HindsightMemory`.
- `MemoryUnit`: returned memory record with content, type, metadata, and bank
  identity.

## Usage

```rust
use hindsight_client::{HindsightMemory, PgHindsightClient};

let memory = PgHindsightClient::new(pool);
let unit_id = memory
    .retain(host_id, bank_id, "User prefers concise answers", "fact", None, metadata)
    .await?;
```

## Data Model

The PostgreSQL implementation writes to `agent_memory_unit_t` and uses
`host_id` plus `bank_id` to isolate memory between tenants, users, or sessions.

## Consumers

`light-agent` uses this crate to persist and recall agent conversation memory.
