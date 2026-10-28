//! Non-process qualification only. Never launches a helper, parent or worker.
use light_lambda_runtime::{qualification_extension_bridge::*, qualification_shutdown::Refusal};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    time::Duration,
};
use tokio::time::Instant;
fn image() -> Image {
    Image {
        bytes: 4,
        sha256: "a".repeat(64),
    }
}
fn manifest() -> Manifest {
    Manifest {
        protocol_version: 1,
        parent: image(),
        extension: image(),
        socket_directory: DIRECTORY.into(),
        owner_requested_bytes: OWNER_BYTES,
        extension_requested_bytes: EXTENSION_BYTES,
        parent_side_envelope_bytes: ENVELOPE_BYTES,
    }
}
#[test]
fn selected_partition_is_charged_once_before_preparation() {
    let m = manifest();
    let charge = EnvelopeCharge::admit(&m, ENVELOPE_BYTES).unwrap();
    assert_eq!(charge.requested(), (224 * 1024 * 1024, 32 * 1024 * 1024));
    assert!(EnvelopeCharge::admit(&m, ENVELOPE_BYTES - 1).is_err());
    assert!(EnvelopeCharge::admit(&m, ENVELOPE_BYTES + 1).is_err());
    for key in 0..3 {
        let mut m = manifest();
        match key {
            0 => m.owner_requested_bytes = u64::MAX,
            1 => m.extension_requested_bytes = 0,
            _ => m.parent_side_envelope_bytes = 0,
        };
        assert!(m.validate().is_err());
    }
}
#[test]
fn bridge_manifest_enforces_role_pins_version_path_and_closed_fields() {
    let m = manifest();
    assert!(m.validate().is_ok());
    for key in 0..5 {
        let mut m = manifest();
        match key {
            0 => m.protocol_version = 2,
            1 => m.parent.bytes = 0,
            2 => m.extension.sha256 = "private-invalid".into(),
            3 => m.socket_directory = "/tmp/other".into(),
            _ => m.parent.sha256 = "A".repeat(64),
        };
        assert_eq!(m.validate().err(), Some(Refusal::Configuration));
    }
    let mut value = serde_json::to_value(m).unwrap();
    value["unreviewed"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<Manifest>(value).is_err());
}
#[test]
fn manifest_hash_symlink_and_write_permissions_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("manifest.json");
    let bytes = serde_json::to_vec(&manifest()).unwrap();
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    assert!(read_manifest(&path, &digest, Instant::now() + Duration::from_secs(2)).is_ok());
    assert!(
        read_manifest(
            &path,
            &"0".repeat(64),
            Instant::now() + Duration::from_secs(2)
        )
        .is_err()
    );
    let link = d.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_manifest(&link, &digest, Instant::now() + Duration::from_secs(2)).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(read_manifest(&path, &digest, Instant::now() + Duration::from_secs(2)).is_err());
    assert!(read_manifest(&path, &digest, Instant::now()).is_err());
}
#[test]
fn malformed_duplicate_oversized_frames_never_become_notifications() {
    for bytes in [
        b"[]".as_slice(),
        b"{}",
        b"{\"type\":\"ack\",\"success\":true,\"extra\":\"private\"}",
        b"{\"type\":\"done\",\"success\":true,\"success\":false}",
    ] {
        assert!(Frame::decode(bytes).is_err());
    }
    assert!(Frame::decode(&vec![b' '; MAX_FRAME + 1]).is_err());
    let good = serde_json::to_vec(&Frame::Done { success: false }).unwrap();
    assert!(matches!(
        Frame::decode(&good).unwrap(),
        Frame::Done { success: false }
    ));
}
#[test]
fn existing_platform_plan_remains_1024_with_no_resource_bound_claim() {
    use light_lambda_runtime::validator_ipc::supervision::Policy;
    let mut p = Policy {
        parent_requested_bytes: ENVELOPE_BYTES,
        worker_requested_bytes: 512 * 1024 * 1024,
        headroom_bytes: 256 * 1024 * 1024,
        platform_bytes: 1024 * 1024 * 1024,
        worker_address_space_bytes: 1024 * 1024 * 1024,
        lifetime_cpu_seconds: 16,
    };
    assert!(validate_policy(&p).is_ok());
    p.parent_requested_bytes -= 1;
    assert!(validate_policy(&p).is_err());
}
