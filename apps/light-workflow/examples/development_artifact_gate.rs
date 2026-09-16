//! Non-production helper: exercise the real filesystem backend across containers.
//! Uses a dedicated namespace/prefix; never reads application artifacts or the DB.
use light_workflow::{
    artifact_publish::ArtifactPublisherStore, artifact_retention::ArtifactObjectStore,
    artifact_store::DurableArtifactStore, configuration::ArtifactSettings,
};
use sha2::{Digest, Sha256};

const HOST: &str = "01a0a138-e838-7825-a9ae-2f9c11aa17f4";
const PREFIX: &str = "phase1-qualification";
const CONTENT: &[u8] = b"phase1 retained evidence\0\xff\x80\n";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?;
    let root = args.get(2).ok_or("absolute filesystem root required")?;
    let settings = ArtifactSettings {
        backend: "filesystem".into(),
        filesystem_root: Some(root.into()),
        bucket: None,
        endpoint: None,
        allow_http: false,
        prefix: PREFIX.into(),
        retention_days: 1,
    };
    let configured = DurableArtifactStore::from_configuration(&settings);
    if mode == "unwritable" && configured.is_err() {
        println!("filesystem gate unwritable: rejected during initialization");
        return Ok(());
    }
    let store = configured?.ok_or("disabled")?;
    let key = format!("{HOST}/restart.bin");
    let staging = format!("object://{PREFIX}/staging/{key}");
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(CONTENT)));
    let raw = digest.strip_prefix("sha256:").unwrap();
    let object = format!("{PREFIX}/tenants/{HOST}/objects/sha256/{}/{raw}", &raw[..2]);
    match mode.as_str() {
        "stage" => {
            store.probe_writable().await?;
            assert_eq!(store.stage(&key, CONTENT).await?, staging);
        }
        "promote" => {
            let first = store.promote(HOST, &staging, &digest).await?;
            assert_eq!(store.promote(HOST, &staging, &digest).await?, first);
            assert_eq!(store.read_verified(HOST, &digest, 1024).await?, CONTENT);
        }
        "read" => {
            assert_eq!(store.read_verified(HOST, &digest, 1024).await?, CONTENT);
            assert!(store.read_verified(HOST, &digest, 1).await.is_err());
            assert!(
                store
                    .read_verified("01a0a138-e838-7825-a9ae-2f9c11aa17f5", &digest, 1024)
                    .await
                    .is_err()
            );
        }
        "corrupt" => {
            std::fs::write(std::path::Path::new(root).join(&object), b"corrupt")?;
            assert!(store.read_verified(HOST, &digest, 1024).await.is_err());
            assert!(store.promote(HOST, &staging, &digest).await.is_err());
        }
        "unwritable" => {
            assert!(store.probe_writable().await.is_err());
        }
        "full" => {
            // This mode must only run in the disposable bounded tmpfs gate.
            assert_eq!(root, "/qualification-evidence");
            use std::io::Write;
            let mut file = std::fs::File::create(format!("{root}/fill"))?;
            let mut full = false;
            for _ in 0..2048 {
                if let Err(error) = file.write_all(&[0; 8192]) {
                    assert_eq!(error.raw_os_error(), Some(28));
                    full = true;
                    break;
                }
            }
            assert!(full, "gate requires a tmpfs no larger than 16 MiB");
            assert!(store.probe_writable().await.is_err());
        }
        "cleanup" => {
            store.delete(&format!("object://{object}")).await?;
            store.delete(&staging).await?;
        }
        _ => return Err("unknown mode".into()),
    }
    println!("filesystem gate {mode}: passed");
    Ok(())
}
