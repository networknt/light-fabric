# Config Loader

`config-loader` loads, merges, resolves, and decrypts service configuration.

It provides the common configuration behavior used by fabric services and
runtime modules. Configuration can be loaded from YAML, JSON, or TOML files,
merged across layers, expanded from values maps, and decrypted when encrypted
values are present.

## Main Types

- `ConfigLoader`: loads files and resolves `${key:default}` style values.
- `ConfigManager<T>`: stores hot-swappable typed configuration behind an
  atomic reference.
- `ConfigError`: shared error type for IO, parse, decrypt, and conversion
  failures.

## Resolution Model

The loader supports:

- merging multiple config files in order
- external overlays through `LIGHT_RS_CONFIG_DIR`
- whole-value variable replacement
- embedded variable expansion inside strings
- typed deserialization through Serde
- symmetric encrypted values through `symmetric-decryptor`
- asymmetric encrypted values through `asymmetric-decryptor`

## Usage

```rust
use config_loader::ConfigLoader;
use std::collections::HashMap;

let loader = ConfigLoader::from_values(HashMap::new(), None, None)?;
let config: MyConfig = loader.load_typed(["config/my-service.yml"])?;
```

## Consumers

`light-runtime` uses this crate for service bootstrap and runtime config.
Application crates can also use it for app-specific policy or domain config.
