use llm_gateway::projection::ProjectionAcknowledgement;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const SENT_DIRECTORY: &str = "sent";

#[derive(Serialize)]
struct PortalCommandEnvelope<'a> {
    host: &'static str,
    service: &'static str,
    action: &'static str,
    version: &'static str,
    data: &'a ProjectionAcknowledgement,
}

#[derive(Debug, Clone)]
pub struct ProjectionAcknowledgementForwarderConfig {
    pub outbox: PathBuf,
    pub endpoint: String,
    pub token_file: PathBuf,
    pub audience: String,
    pub poll_interval: Duration,
}

pub fn start_projection_acknowledgement_forwarder(
    config: ProjectionAcknowledgementForwarderConfig,
) -> Result<JoinHandle<()>, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| format!("cannot build projection acknowledgement client: {error}"))?;
    Ok(tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(config.poll_interval.max(Duration::from_millis(250)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let files = match pending_files(&config.outbox) {
                Ok(files) => files,
                Err(error) => {
                    warn!(error = %error, "cannot scan the LLM acknowledgement outbox");
                    continue;
                }
            };
            for path in files {
                if let Err(error) = forward_one(&client, &config, &path).await {
                    warn!(path = %path.display(), error = %error,
                        "LLM acknowledgement remains in the durable outbox");
                }
            }
        }
    }))
}

async fn forward_one(
    client: &reqwest::Client,
    config: &ProjectionAcknowledgementForwarderConfig,
    path: &Path,
) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("cannot read outbox entry: {error}"))?;
    let acknowledgement: ProjectionAcknowledgement = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid acknowledgement envelope: {error}"))?;
    match acknowledgement_disposition(&acknowledgement)? {
        AcknowledgementDisposition::Forward => {}
        AcknowledgementDisposition::ArchiveLegacyV2 => {
            archive(&config.outbox, path)?;
            debug!(path = %path.display(),
                "archived a legacy v2 acknowledgement that has no Portal publication identity");
            return Ok(());
        }
    }
    let token = fs::read_to_string(&config.token_file)
        .map_err(|error| format!("cannot read projected workload token: {error}"))?;
    let token = token.trim();
    if token.is_empty() {
        return Err("projected workload token is empty".to_string());
    }
    let command = portal_command_bytes(&acknowledgement)?;
    let response = client
        .post(&config.endpoint)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(
            "x-llm-acknowledgement-digest",
            &acknowledgement.acknowledgement_digest,
        )
        .header("x-llm-acknowledgement-audience", &config.audience)
        .body(command)
        .send()
        .await
        .map_err(|error| format!("Portal acknowledgement request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Portal rejected acknowledgement with {}",
            response.status()
        ));
    }
    archive(&config.outbox, path)?;
    debug!(path = %path.display(), "archived delivered LLM acknowledgement");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcknowledgementDisposition {
    Forward,
    ArchiveLegacyV2,
}

fn acknowledgement_disposition(
    acknowledgement: &ProjectionAcknowledgement,
) -> Result<AcknowledgementDisposition, String> {
    if !acknowledgement.verify_digest() {
        return Err("acknowledgement digest is invalid".to_string());
    }
    if acknowledgement.gateway_publication_id.trim().is_empty() {
        return if acknowledgement.applied_schema_version == "2" {
            Ok(AcknowledgementDisposition::ArchiveLegacyV2)
        } else {
            Err("acknowledgement publication identity is invalid".to_string())
        };
    }
    Ok(AcknowledgementDisposition::Forward)
}

fn portal_command_bytes(acknowledgement: &ProjectionAcknowledgement) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&PortalCommandEnvelope {
        host: "lightapi.net",
        service: "genai",
        action: "acknowledgeLlmGatewayPublicationReplica",
        version: "0.1.0",
        data: acknowledgement,
    })
    .map_err(|error| format!("cannot encode Portal acknowledgement command: {error}"))
}

fn pending_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let sent = root.join(SENT_DIRECTORY);
    let mut files = Vec::new();
    for publication in fs::read_dir(root).map_err(|error| error.to_string())? {
        let publication = publication.map_err(|error| error.to_string())?;
        let path = publication.path();
        if path == sent || !path.is_dir() {
            continue;
        }
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let entry_path = entry.path();
            if entry_path.extension().is_some_and(|value| value == "json") && entry_path.is_file() {
                files.push(entry_path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn archive(root: &Path, path: &Path) -> Result<(), String> {
    let publication = path
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| "outbox publication directory is missing".to_string())?;
    let destination = root.join(SENT_DIRECTORY).join(publication).join(
        path.file_name()
            .ok_or_else(|| "outbox file name is missing".to_string())?,
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let source_directory = path.parent().map(Path::to_path_buf);
    fs::rename(path, destination).map_err(|error| error.to_string())?;
    if let Some(source_directory) = source_directory {
        match fs::remove_dir(source_directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_gateway::projection::PROJECTION_ACKNOWLEDGEMENT_SCHEMA_VERSION;

    fn acknowledgement() -> ProjectionAcknowledgement {
        let mut value = ProjectionAcknowledgement {
            schema_version: PROJECTION_ACKNOWLEDGEMENT_SCHEMA_VERSION.to_string(),
            gateway_publication_id: "publication-a".to_string(),
            host_id: "host-a".to_string(),
            environment: "prod".to_string(),
            inventory_id: "inventory-a".to_string(),
            inventory_generation: 1,
            inventory_digest: "a".repeat(64),
            sequence: 7,
            root_digest: "b".repeat(64),
            applied_schema_version: "3".to_string(),
            applied_at: "2026-08-08T12:00:00Z".to_string(),
            gateway_version: "0.2.1".to_string(),
            reader_version: "projection-reader-v3".to_string(),
            gateway_instance: "pod-a".to_string(),
            material_generation: 3,
            resolved_trust_digest: "c".repeat(64),
            evidence_key_set_version: "keys-v2".to_string(),
            evidence_key_set_digest: "d".repeat(64),
            state: "applied".to_string(),
            failure_category: None,
            failure_code: None,
            acknowledgement_digest: String::new(),
        };
        value.refresh_digest().unwrap();
        value
    }

    #[test]
    fn pending_scan_is_publication_scoped_and_archive_is_recoverable() {
        let directory = tempfile::tempdir().unwrap();
        let publication = directory.path().join("publication-7-root");
        fs::create_dir_all(&publication).unwrap();
        let path = publication.join("pod-a.json");
        fs::write(&path, serde_json::to_vec(&acknowledgement()).unwrap()).unwrap();
        assert_eq!(pending_files(directory.path()).unwrap(), vec![path.clone()]);
        archive(directory.path(), &path).unwrap();
        assert!(
            directory
                .path()
                .join("sent/publication-7-root/pod-a.json")
                .is_file()
        );
        assert!(pending_files(directory.path()).unwrap().is_empty());
        assert!(!publication.exists());
    }

    #[test]
    fn acknowledgement_uses_the_typed_portal_command_envelope() {
        let value: serde_json::Value =
            serde_json::from_slice(&portal_command_bytes(&acknowledgement()).unwrap()).unwrap();
        assert_eq!(value["action"], "acknowledgeLlmGatewayPublicationReplica");
        assert_eq!(value["version"], "0.1.0");
        assert_eq!(value["data"]["readerVersion"], "projection-reader-v3");
    }

    #[test]
    fn legacy_v2_acknowledgement_without_publication_id_is_archived_not_retried() {
        let mut value = acknowledgement();
        value.applied_schema_version = "2".to_string();
        value.gateway_publication_id.clear();
        value.refresh_digest().unwrap();
        assert_eq!(
            acknowledgement_disposition(&value).unwrap(),
            AcknowledgementDisposition::ArchiveLegacyV2
        );

        value.applied_schema_version = "3".to_string();
        value.refresh_digest().unwrap();
        assert!(acknowledgement_disposition(&value).is_err());
    }
}
