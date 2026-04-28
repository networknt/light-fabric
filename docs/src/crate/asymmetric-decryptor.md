# Asymmetric Decryptor

`asymmetric-decryptor` decrypts RSA encrypted configuration values.

It is used by `config-loader` when a service loads encrypted values that use
the `CRYPT:RSA:` prefix. The crate supports RSA private keys in PKCS#8 and
PKCS#1 PEM formats and decrypts payloads with RSA-OAEP using SHA-256.

## Main Types

- `AsymmetricDecryptor`: owns the RSA private key and decrypts supported
  payloads.
- `AsymmetricError`: error type for prefix, base64, key, and decrypt failures.
- `CRYPT_RSA_PREFIX`: the required `CRYPT:RSA:` payload prefix.

## Usage

```rust
use asymmetric_decryptor::AsymmetricDecryptor;

let decryptor = AsymmetricDecryptor::from_pem(private_key_pem)?;
let plaintext = decryptor.decrypt("CRYPT:RSA:...")?;
```

## Notes

This crate is intentionally small. It does not fetch keys, rotate keys, or
perform configuration merging. Those concerns belong to `config-loader` and the
runtime layer.
