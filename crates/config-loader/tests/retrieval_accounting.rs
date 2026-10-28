//! Synthetic credentials only. Each environment is owned by a child process;
//! the test never mutates the parent environment or uses native getenv.
use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::Engine;
use config_loader::{ConfigLoader, RuleBudget};
use serde_yaml::Value;
use std::{collections::HashMap, process::Command};

fn aes_fixture() -> String {
    let salt = [1u8; 16];
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"owned-test-password", &salt, 65536, &mut key);
    let mut data = b"false".to_vec();
    let len = data.len();
    data.resize(len + 16, 0);
    let ciphertext = cbc::Encryptor::<aes::Aes256>::new(&key.into(), &[0u8; 16].into())
        .encrypt_padded_mut::<Pkcs7>(&mut data, len)
        .unwrap();
    format!("CRYPT:{}:{}", hex::encode(salt), hex::encode(ciphertext))
}

#[test]
fn safe_unicode_environment_and_aes_rsa_retrieval_match_legacy() {
    let rsa = format!(
        "CRYPT:RSA:{}",
        base64::engine::general_purpose::STANDARD.encode(include_bytes!("data/wp12-owned-rsa.bin"))
    );
    for normalized in [true, false] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "owned_environment_child", "--nocapture"])
            .env_clear()
            .env("WP12_RETRIEVAL_CHILD", "1")
            .env("Test.é-Value", "second-λ")
            .env(
                "WP12_EXPECTED",
                if normalized { "first-λ" } else { "second-λ" },
            )
            .env("WP12_AES", aes_fixture())
            .env("WP12_RSA", &rsa)
            .env("WP12_LARGE", "x".repeat(RuleBudget::BYTES + 1));
        if normalized {
            child.env("TEST_É_VALUE", "first-λ");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            child.env("WP12_NONUNICODE", std::ffi::OsString::from_vec(vec![255]));
        }
        assert!(child.status().unwrap().success());
    }
}

#[test]
fn owned_environment_child() {
    if std::env::var("WP12_RETRIEVAL_CHILD").as_deref() != Ok("1") {
        return;
    }
    let loader = ConfigLoader::from_values(
        HashMap::new(),
        Some("owned-test-password"),
        Some(include_str!("data/wp12-owned-rsa.pem")),
    )
    .unwrap();
    for text in [
        "${Test.é-Value}",
        "${WP12_AES}",
        "${WP12_RSA}",
        "${absent:λ}",
        "${WP12_NONUNICODE:fallback}",
        "x${Test.é-Value}y",
    ] {
        let mut legacy = Value::String(text.into());
        let mut bounded = legacy.clone();
        loader.resolve_value(&mut legacy).unwrap();
        loader
            .resolve_value_bounded(&mut bounded, &HashMap::new())
            .unwrap();
        assert_eq!(bounded, legacy, "{text}");
        if text == "${Test.é-Value}" {
            assert_eq!(
                bounded.as_str(),
                Some(std::env::var("WP12_EXPECTED").unwrap().as_str())
            );
        }
        if text == "${WP12_AES}" || text == "${WP12_RSA}" {
            assert_eq!(bounded, Value::Bool(false));
        }
    }
    let mut large = Value::String("${WP12_LARGE}".into());
    assert!(
        loader
            .resolve_value_bounded(&mut large, &HashMap::new())
            .is_err()
    );
    assert_eq!(large.as_str(), Some("${WP12_LARGE}"));
}
