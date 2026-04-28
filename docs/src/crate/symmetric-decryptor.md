# Symmetric Decryptor

`symmetric-decryptor` decrypts legacy symmetric encrypted configuration values.

It supports payloads with the `CRYPT` prefix and decrypts AES-256-CBC data with
a key derived from the configured password using PBKDF2-HMAC-SHA256.

## Main Types

- `Decryptor`: trait implemented by decryptors.
- `SymmetricDecryptor`: password-based decryptor.
- `DecryptError`: error type for prefix, format, hex, and cipher failures.
- `CRYPT_PREFIX`: required `CRYPT` payload prefix.

## Usage

```rust
use symmetric_decryptor::{Decryptor, SymmetricDecryptor};

let decryptor = SymmetricDecryptor::new("password");
let plaintext = decryptor.decrypt("CRYPT:...")?;
```

## Consumers

`config-loader` uses this crate when it encounters symmetric encrypted values
and a config password is available.
