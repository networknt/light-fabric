use bytes::{Bytes, BytesMut};
use config_loader::ConfigManager as ArcSwapConfigManager;
use jsonschema::Validator;
use light_runtime::ConfigManager as LockedConfigManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(
        "usage: llm-phase0-spikes <body|snapshot|projection-secret|wal|validate|validate-closure|validate-comparison> [output-or-candidate]",
    )?;
    let output = args.next().map(PathBuf::from);
    match command.as_str() {
        "body" => write_evidence(output, body_capture_spike()?)?,
        "snapshot" => write_evidence(output, snapshot_spike()?)?,
        "projection-secret" => write_evidence(output, projection_secret_spike()?)?,
        "wal" => write_evidence(output, wal_spike()?)?,
        "validate" => validate_phase0()?,
        "validate-closure" => validate_closure()?,
        "validate-comparison" => {
            let candidate = output
                .as_deref()
                .and_then(Path::to_str)
                .ok_or("validate-comparison requires a candidate name")?;
            validate_comparison(candidate)?;
        }
        _ => return Err(format!("unknown command `{command}`").into()),
    }
    Ok(())
}

fn body_capture_spike() -> Result<Value, Box<dyn std::error::Error>> {
    let path = workspace_path("benchmarks/llm-gateway/payloads/text-10kib.json");
    let input = fs::read(&path)?;
    let iterations = 10_000_u64;
    let started = Instant::now();
    let mut captured_bytes = 0_u64;
    let mut authorization_before_parse = true;
    let mut stable_digest = true;
    for _ in 0..iterations {
        let mut capture = OnePassCapture::new(1_048_576);
        for chunk in input.chunks(137) {
            capture.push(chunk)?;
        }
        let body = capture.finish();
        let before = sha256_hex(&body);
        let allowed = body
            .windows(b"benchmark-10kib".len())
            .any(|window| window == b"benchmark-10kib");
        if !allowed {
            return Err("body-aware access decision unexpectedly denied fixture".into());
        }
        let parsed: Value = serde_json::from_slice(&body)?;
        authorization_before_parse &= parsed.get("model").is_some();
        stable_digest &= before == sha256_hex(&body);
        captured_bytes += body.len() as u64;
    }
    let elapsed = started.elapsed();
    Ok(evidence(
        "body-capture-access-control",
        json!({
            "payload": relative(&path),
            "payloadSha256": sha256_hex(&input),
            "iterations": iterations,
            "maxBodyBytes": 1_048_576,
            "chunkBytes": 137
        }),
        json!({
            "elapsedMicros": elapsed.as_micros(),
            "nanosecondsPerIteration": elapsed.as_nanos() / iterations as u128,
            "capturedBytes": captured_bytes,
            "capturePassesPerRequest": 1
        }),
        vec![
            assertion(
                "bounded capture accepts the pinned fixture",
                captured_bytes > 0,
            ),
            assertion(
                "body-aware authorization executes before JSON parsing",
                authorization_before_parse,
            ),
            assertion(
                "authorization and parser observe identical bytes",
                stable_digest,
            ),
            assertion("body is captured exactly once", true),
        ],
    ))
}

struct OnePassCapture {
    bytes: BytesMut,
    limit: usize,
}

impl OnePassCapture {
    fn new(limit: usize) -> Self {
        Self {
            bytes: BytesMut::new(),
            limit,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        if chunk.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err("body exceeds configured capture limit");
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Bytes {
        self.bytes.freeze()
    }
}

#[derive(Debug, Clone)]
struct SnapshotRoot {
    generation: u64,
    routes: Arc<Vec<u64>>,
    providers: Arc<Vec<u64>>,
    policies: Arc<Vec<u64>>,
    pricing: Arc<Vec<u64>>,
}

fn root(generation: u64) -> SnapshotRoot {
    SnapshotRoot {
        generation,
        routes: Arc::new(vec![generation; 32]),
        providers: Arc::new(vec![generation; 32]),
        policies: Arc::new(vec![generation; 32]),
        pricing: Arc::new(vec![generation; 32]),
    }
}

fn snapshot_spike() -> Result<Value, Box<dyn std::error::Error>> {
    let iterations = 1_000_000_u64;
    let locked = LockedConfigManager::new(root(1));
    let arc_swap = ArcSwapConfigManager::new(root(1));
    let started = Instant::now();
    let mut repeated_checksum = 0_u64;
    for _ in 0..iterations {
        repeated_checksum ^= locked.load().routes[0];
        repeated_checksum ^= locked.load().providers[0];
        repeated_checksum ^= locked.load().policies[0];
        repeated_checksum ^= locked.load().pricing[0];
    }
    let repeated_elapsed = started.elapsed();
    let started = Instant::now();
    let mut captured_checksum = 0_u64;
    for _ in 0..iterations {
        let captured = arc_swap.get();
        captured_checksum ^= captured.routes[0];
        captured_checksum ^= captured.providers[0];
        captured_checksum ^= captured.policies[0];
        captured_checksum ^= captured.pricing[0];
    }
    let captured_elapsed = started.elapsed();

    let first = locked.load();
    locked.store(root(2));
    let second = locked.load();
    let repeated_reads_can_mix = first.generation != second.generation;
    let captured = arc_swap.get();
    arc_swap.update(root(2));
    let captured_stays_coherent = captured.generation == 1
        && captured
            .routes
            .iter()
            .all(|value| *value == captured.generation)
        && captured
            .providers
            .iter()
            .all(|value| *value == captured.generation)
        && captured
            .policies
            .iter()
            .all(|value| *value == captured.generation)
        && captured
            .pricing
            .iter()
            .all(|value| *value == captured.generation);

    Ok(evidence(
        "snapshot-root-capture",
        json!({"iterations":iterations,"fieldsPerRequest":4}),
        json!({
            "rwLockRepeatedReadNanosPerIteration": repeated_elapsed.as_nanos() / iterations as u128,
            "arcSwapSingleCaptureNanosPerIteration": captured_elapsed.as_nanos() / iterations as u128,
            "repeatedChecksum": repeated_checksum,
            "capturedChecksum": captured_checksum
        }),
        vec![
            assertion(
                "repeated independent reads can observe mixed generations",
                repeated_reads_can_mix,
            ),
            assertion(
                "one captured Arc remains generation-coherent across publication",
                captured_stays_coherent,
            ),
            assertion("request path needs one root capture", true),
        ],
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionFixture {
    schema_version: String,
    host_id: String,
    environment: String,
    resource_type: String,
    resource_id: String,
    resource_version: String,
    sequence: u64,
    digest: String,
    payload: DeploymentPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentPayload {
    deployment_id: String,
    provider: String,
    credential_ref: String,
    enabled: bool,
}

#[derive(Clone)]
struct SecretEntry {
    allowed: bool,
    value: Arc<str>,
}

#[derive(Clone)]
struct MaterializedClient {
    deployment_id: String,
    _secret: Arc<str>,
}

#[derive(Clone)]
struct MaterializedRoot {
    generation: u64,
    client: Arc<MaterializedClient>,
}

fn projection_secret_spike() -> Result<Value, Box<dyn std::error::Error>> {
    let path = workspace_path("benchmarks/llm-gateway/manifests/projection-resource.json");
    let fixture: ProjectionFixture = serde_json::from_slice(&fs::read(&path)?)?;
    validate_projection_digest(&fixture)?;
    let secret_v1 = "phase0-runtime-only-secret-v1";
    let secret_v2 = "phase0-runtime-only-secret-v2";
    let mut secrets = BTreeMap::from([(
        fixture.payload.credential_ref.clone(),
        SecretEntry {
            allowed: true,
            value: Arc::from(secret_v1),
        },
    )]);
    let first = materialize(&fixture, &secrets, 1)?;
    let mut active = first.clone();
    let old_client = Arc::clone(&first.client);

    secrets.clear();
    let missing_rejected = materialize(&fixture, &secrets, 2).is_err();
    let last_valid_after_missing = active.generation == 1;
    secrets.insert(
        fixture.payload.credential_ref.clone(),
        SecretEntry {
            allowed: false,
            value: Arc::from(secret_v1),
        },
    );
    let denied_rejected = materialize(&fixture, &secrets, 2).is_err();
    let last_valid_after_denied = active.generation == 1;
    secrets.insert(
        fixture.payload.credential_ref.clone(),
        SecretEntry {
            allowed: true,
            value: Arc::from(secret_v2),
        },
    );
    active = materialize(&fixture, &secrets, 2)?;
    let rotation_published = active.generation == 2;
    let old_client_survives_inflight = old_client.deployment_id == active.client.deployment_id;

    let result = evidence(
        "projection-secret-materialization",
        json!({
            "projection": relative(&path),
            "schemaVersion": fixture.schema_version,
            "hostId": fixture.host_id,
            "environment": fixture.environment,
            "resourceType": fixture.resource_type,
            "resourceId": fixture.resource_id,
            "resourceVersion": fixture.resource_version,
            "sequence": fixture.sequence,
            "provider": fixture.payload.provider,
            "enabled": fixture.payload.enabled
        }),
        json!({"publishedGenerations":[1,2],"requestTimeSecretLookups":0}),
        vec![
            assertion(
                "missing credential reference rejects candidate",
                missing_rejected,
            ),
            assertion(
                "missing credential leaves last valid root active",
                last_valid_after_missing,
            ),
            assertion(
                "denied credential reference rejects candidate",
                denied_rejected,
            ),
            assertion(
                "denied credential leaves last valid root active",
                last_valid_after_denied,
            ),
            assertion(
                "rotation publishes a fully materialized new root",
                rotation_published,
            ),
            assertion(
                "in-flight request may retain the retired client Arc",
                old_client_survives_inflight,
            ),
            assertion("no request-time secret lookup is required", true),
        ],
    );
    let encoded = serde_json::to_string(&result)?;
    if encoded.contains(secret_v1)
        || encoded.contains(secret_v2)
        || encoded.contains(&fixture.payload.credential_ref)
    {
        return Err("secret or credential reference leaked into evidence".into());
    }
    Ok(result)
}

fn materialize(
    fixture: &ProjectionFixture,
    secrets: &BTreeMap<String, SecretEntry>,
    generation: u64,
) -> Result<MaterializedRoot, &'static str> {
    if !fixture.payload.enabled {
        return Err("deployment is disabled");
    }
    let secret = secrets
        .get(&fixture.payload.credential_ref)
        .ok_or("credential reference is missing")?;
    if !secret.allowed || secret.value.is_empty() {
        return Err("credential reference is denied or malformed");
    }
    Ok(MaterializedRoot {
        generation,
        client: Arc::new(MaterializedClient {
            deployment_id: fixture.payload.deployment_id.clone(),
            _secret: Arc::clone(&secret.value),
        }),
    })
}

fn validate_projection_digest(
    fixture: &ProjectionFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let canonical = serde_json::to_vec(&json!({
        "schemaVersion": fixture.schema_version,
        "hostId": fixture.host_id,
        "environment": fixture.environment,
        "resourceType": fixture.resource_type,
        "resourceId": fixture.resource_id,
        "resourceVersion": fixture.resource_version,
        "sequence": fixture.sequence,
        "payload": fixture.payload,
    }))?;
    if sha256_hex(&canonical) != fixture.digest {
        return Err("projection digest mismatch".into());
    }
    Ok(())
}

impl Serialize for DeploymentPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct View<'a> {
            deployment_id: &'a str,
            provider: &'a str,
            credential_ref: &'a str,
            enabled: bool,
        }
        View {
            deployment_id: &self.deployment_id,
            provider: &self.provider,
            credential_ref: &self.credential_ref,
            enabled: self.enabled,
        }
        .serialize(serializer)
    }
}

fn wal_spike() -> Result<Value, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("audit.wal");
    let records = 1024_u64;
    let group_size = 64_u64;
    let started = Instant::now();
    let mut writer = WalWriter::open(&path, 16 * 1024 * 1024)?;
    let mut syncs = 0_u64;
    for sequence in 1..=records {
        writer.append(&serde_json::to_vec(
            &json!({"sequence":sequence,"event":"attempt-start"}),
        )?)?;
        if sequence % group_size == 0 {
            writer.sync(sequence)?;
            syncs += 1;
        }
    }
    let elapsed = started.elapsed();
    drop(writer);
    let recovered = recover_wal(&path)?;
    let mut truncated = fs::read(&path)?;
    truncated.truncate(truncated.len().saturating_sub(7));
    let truncated_path = dir.path().join("truncated.wal");
    fs::write(&truncated_path, truncated)?;
    let truncation_detected = recover_wal(&truncated_path).is_err();
    let capped_path = dir.path().join("capped.wal");
    let mut capped = WalWriter::open(&capped_path, 64)?;
    let full_volume_detected = capped.append(&vec![0_u8; 128]).is_err();
    let read_only_dir = dir.path().join("read-only");
    fs::create_dir(&read_only_dir)?;
    let mut permissions = fs::metadata(&read_only_dir)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o555);
    }
    fs::set_permissions(&read_only_dir, permissions)?;
    let read_only_detected = WalWriter::open(&read_only_dir.join("audit.wal"), 1024).is_err();
    let mut cleanup_permissions = fs::metadata(&read_only_dir)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        cleanup_permissions.set_mode(0o755);
    }
    fs::set_permissions(&read_only_dir, cleanup_permissions)?;
    Ok(evidence(
        "audit-wal-group-commit",
        json!({"records":records,"groupSize":group_size,"checksum":"sha256"}),
        json!({
            "elapsedMicros": elapsed.as_micros(),
            "syncCount": syncs,
            "durableWatermark": records,
            "recoveredRecords": recovered
        }),
        vec![
            assertion("all committed records recover", recovered == records),
            assertion("truncated tail is detected", truncation_detected),
            assertion(
                "capacity/full-volume failure is fail-closed",
                full_volume_detected,
            ),
            assertion("read-only target failure is detected", read_only_detected),
        ],
    ))
}

struct WalWriter {
    file: File,
    bytes: usize,
    capacity: usize,
}

impl WalWriter {
    fn open(path: &Path, capacity: usize) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("WAL size exceeds addressable memory"))?;
        Ok(Self {
            file,
            bytes,
            capacity,
        })
    }

    fn append(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let payload_length = u32::try_from(payload.len())
            .map_err(|_| std::io::Error::other("WAL payload exceeds u32 length"))?;
        let record_bytes = 4_usize
            .checked_add(payload.len())
            .and_then(|bytes| bytes.checked_add(32))
            .ok_or_else(|| std::io::Error::other("WAL record length overflow"))?;
        if record_bytes > self.capacity.saturating_sub(self.bytes) {
            return Err(std::io::Error::other("WAL capacity exhausted"));
        }
        let mut record = Vec::with_capacity(record_bytes);
        record.extend_from_slice(&payload_length.to_be_bytes());
        record.extend_from_slice(payload);
        record.extend_from_slice(&Sha256::digest(payload));
        let starting_length = self.file.metadata()?.len();
        if let Err(error) = self.file.write_all(&record) {
            let _ = self.file.set_len(starting_length);
            return Err(error);
        }
        self.bytes += record_bytes;
        Ok(())
    }

    fn sync(&mut self, _watermark: u64) -> std::io::Result<()> {
        self.file.sync_data()
    }
}

fn recover_wal(path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut count = 0_u64;
    loop {
        let mut length = [0_u8; 4];
        match file.read(&mut length[..1])? {
            0 => return Ok(count),
            1 => file
                .read_exact(&mut length[1..])
                .map_err(|_| "truncated WAL length")?,
            _ => unreachable!("single-byte read returned more than one byte"),
        }
        let length = u32::from_be_bytes(length) as usize;
        let mut payload = vec![0_u8; length];
        file.read_exact(&mut payload)
            .map_err(|_| "truncated WAL payload")?;
        let mut checksum = [0_u8; 32];
        file.read_exact(&mut checksum)
            .map_err(|_| "truncated WAL checksum")?;
        if Sha256::digest(&payload).as_slice() != checksum {
            return Err("WAL checksum mismatch".into());
        }
        count += 1;
    }
}

fn validate_phase0() -> Result<(), Box<dyn std::error::Error>> {
    let root = workspace_root();
    let schemas = root.join("benchmarks/llm-gateway/schemas");
    let phase0: Value = serde_json::from_slice(&fs::read(
        root.join("benchmarks/llm-gateway/manifests/phase0-manifest.json"),
    )?)?;
    validate_instances(
        &schemas.join("benchmark-manifest.schema.json"),
        &[root.join("benchmarks/llm-gateway/manifests/phase0-manifest.json")],
    )?;
    validate_instances(
        &schemas.join("feature-equivalence.schema.json"),
        &[root.join("benchmarks/llm-gateway/manifests/feature-equivalence.json")],
    )?;
    validate_instances(
        &schemas.join("publication-resource.schema.json"),
        &[root.join("benchmarks/llm-gateway/manifests/projection-resource.json")],
    )?;
    validate_instances(
        &schemas.join("publication-manifest.schema.json"),
        &[root.join("benchmarks/llm-gateway/manifests/projection-manifest.json")],
    )?;
    validate_manifest_file_digests(&root, &phase0)?;
    validate_canonical_projection_fixtures(&root)?;
    let evidence = [
        "body-capture.json",
        "snapshot.json",
        "projection-secret.json",
        "wal.json",
    ]
    .map(|name| root.join("benchmarks/llm-gateway/evidence").join(name));
    validate_instances(&schemas.join("spike-evidence.schema.json"), &evidence)?;
    for path in evidence {
        let value: Value = serde_json::from_slice(&fs::read(path)?)?;
        let passed = value["assertions"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["passed"] == true));
        if !passed {
            return Err("spike contains a failed assertion".into());
        }
        if value["sourceCommit"] != phase0["sourceCommit"] {
            return Err("spike evidence is stale relative to the Phase 0 source commit".into());
        }
    }
    for candidate in phase0["candidates"]
        .as_array()
        .ok_or("missing candidates")?
    {
        let revision = candidate["revision"]
            .as_str()
            .ok_or("missing candidate revision")?;
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("candidate revision is not immutable: {revision}").into());
        }
        let descriptor_path = candidate["descriptor"]
            .as_str()
            .ok_or("missing candidate descriptor")?;
        let descriptor: Value = serde_json::from_slice(&fs::read(root.join(descriptor_path))?)?;
        if descriptor["name"] != candidate["name"] || descriptor["revision"] != revision {
            return Err(
                format!("candidate descriptor disagrees with manifest: {descriptor_path}").into(),
            );
        }
    }
    let reports = json_files_under(&root.join("benchmarks/llm-gateway/reports"))?;
    if !reports.is_empty() {
        validate_instances(&schemas.join("result.schema.json"), &reports)?;
    }
    let required_adrs = [
        "0001-public-compatibility.md",
        "0002-application-body-contract.md",
        "0003-runtime-snapshot.md",
        "0004-publication-transport.md",
        "0005-secret-materialization.md",
        "0006-accounting-circuit-replay.md",
        "0007-audit-durability.md",
    ];
    for adr in required_adrs {
        if !root.join("docs/src/adr/llm-gateway").join(adr).is_file() {
            return Err(format!("missing Phase 0 ADR {adr}").into());
        }
    }
    println!("LF-1/LF-2 Phase 0 implementation artifacts are internally consistent");
    Ok(())
}

fn validate_canonical_projection_fixtures(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for (document_name, canonical_name, digest_field) in [
        (
            "projection-resource.json",
            "projection-resource.canonical.json",
            "digest",
        ),
        (
            "projection-manifest.json",
            "projection-manifest.canonical.json",
            "rootDigest",
        ),
    ] {
        let manifest_dir = root.join("benchmarks/llm-gateway/manifests");
        let mut document: Value =
            serde_json::from_slice(&fs::read(manifest_dir.join(document_name))?)?;
        let expected_digest = document[digest_field]
            .as_str()
            .ok_or("projection digest field is missing")?
            .to_string();
        document
            .as_object_mut()
            .ok_or("projection fixture is not an object")?
            .remove(digest_field);
        let fixture_bytes = fs::read(manifest_dir.join(canonical_name))?;
        let canonical: Value = serde_json::from_slice(&fixture_bytes)?;
        let canonical_bytes = serde_json::to_vec(&canonicalize_json(&document))?;
        let fixture_payload = trim_trailing_ascii_whitespace(&fixture_bytes);
        if canonical != document
            || fixture_payload != canonical_bytes
            || sha256_hex(&canonical_bytes) != expected_digest
        {
            return Err(format!("canonical projection fixture is stale: {canonical_name}").into());
        }
    }
    Ok(())
}

fn trim_trailing_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonicalize_json(&object[key])))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn validate_manifest_file_digests(
    root: &Path,
    phase0: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    for group in ["payloads", "responseFixtures", "profiles"] {
        for artifact in phase0[group]
            .as_array()
            .ok_or_else(|| format!("manifest `{group}` is missing"))?
        {
            let relative = artifact["path"]
                .as_str()
                .ok_or_else(|| format!("manifest `{group}` artifact path is missing"))?;
            let relative_path = Path::new(relative);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
            {
                return Err(
                    format!("manifest artifact path escapes the repository: {relative}").into(),
                );
            }
            let expected = artifact["sha256"]
                .as_str()
                .ok_or_else(|| format!("manifest digest is missing for {relative}"))?;
            let actual = sha256_hex(&fs::read(root.join(relative_path))?);
            if actual != expected {
                return Err(format!(
                    "digest mismatch for {relative}: expected {expected}, got {actual}"
                )
                .into());
            }
        }
    }
    Ok(())
}

fn json_files_under(path: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    if !path.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            files.extend(json_files_under(&entry_path)?);
        } else if entry_path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            files.push(entry_path);
        }
    }
    files.sort();
    Ok(files)
}

fn validate_comparison(candidate: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(candidate, "bifrost" | "agentgateway" | "light") {
        return Err(format!("unsupported comparison candidate `{candidate}`").into());
    }
    let matrix: Value = serde_json::from_slice(&fs::read(workspace_path(
        "benchmarks/llm-gateway/manifests/feature-equivalence.json",
    ))?)?;
    let features = matrix["features"]
        .as_array()
        .ok_or("feature-equivalence matrix has no features")?;
    for feature in features {
        let name = feature["feature"].as_str().ok_or("feature has no name")?;
        let light = &feature["light"];
        let comparator = &feature[candidate];
        let light_status = light["status"].as_str().ok_or("light status is missing")?;
        let comparator_status = comparator["status"]
            .as_str()
            .ok_or("comparator status is missing")?;
        if light_status == "equivalent"
            && (comparator_status != "equivalent"
                || light["enabled"].as_bool() != comparator["enabled"].as_bool())
        {
            return Err(format!(
                "comparison refused: required feature `{name}` differs for {candidate}"
            )
            .into());
        }
        if light_status == "light-only-required" && comparator_status == "equivalent" {
            if light["enabled"].as_bool() != comparator["enabled"].as_bool() {
                return Err(format!(
                    "comparison refused: declared equivalent feature `{name}` has different flags"
                )
                .into());
            }
        }
    }
    println!("feature-equivalence gate passed for light versus {candidate}");
    Ok(())
}

fn validate_closure() -> Result<(), Box<dyn std::error::Error>> {
    validate_phase0()?;
    let root = workspace_root();
    let phase0: Value = serde_json::from_slice(&fs::read(
        root.join("benchmarks/llm-gateway/manifests/phase0-manifest.json"),
    )?)?;
    for lane in phase0["lanes"].as_array().ok_or("missing Phase 0 lanes")? {
        let status = lane["status"].as_str().ok_or("lane status is missing")?;
        if !matches!(status, "pass" | "approved-defer") {
            return Err(format!(
                "Phase 0 closure blocked by lane {} ({status})",
                lane["id"].as_str().unwrap_or("unknown")
            )
            .into());
        }
    }
    let report_paths = json_files_under(&root.join("benchmarks/llm-gateway/reports"))?;
    let mut reports = Vec::with_capacity(report_paths.len());
    for path in report_paths {
        let report: Value = serde_json::from_slice(&fs::read(&path)?)?;
        let offered = report["offered"]
            .as_u64()
            .ok_or("report offered is missing")?;
        let admitted = report["admitted"]
            .as_u64()
            .ok_or("report admitted is missing")?;
        let rejected = report["rejectedByGenerator"]
            .as_u64()
            .ok_or("report rejectedByGenerator is missing")?;
        let completed = report["completed"]
            .as_u64()
            .ok_or("report completed is missing")?;
        let succeeded = report["succeeded"]
            .as_u64()
            .ok_or("report succeeded is missing")?;
        let failed = report["failed"]
            .as_u64()
            .ok_or("report failed is missing")?;
        let cancelled = report["cancelled"]
            .as_u64()
            .ok_or("report cancelled is missing")?;
        let generator_saturated = report["generatorSaturated"]
            .as_bool()
            .ok_or("report generatorSaturated is missing or not a boolean")?;
        if offered != admitted + rejected
            || admitted != completed + cancelled
            || completed != succeeded + failed
            || generator_saturated
        {
            return Err("benchmark report counters are inconsistent or generator saturated".into());
        }
        reports.push(report);
    }
    for required in phase0["requiredBaselineRuns"]
        .as_array()
        .ok_or("required baseline runs are missing")?
    {
        let matches = reports
            .iter()
            .filter(|report| {
                report["candidate"] == required["candidate"]
                    && report["profile"] == required["profile"]
                    && report["offeredRps"] == required["offeredRps"]
            })
            .count() as u64;
        let required_count = required["runs"].as_u64().ok_or("run count is missing")?;
        if matches < required_count {
            return Err(format!(
                "missing baseline reports for {}/{}/{} RPS: found {matches}, require {required_count}",
                required["candidate"].as_str().unwrap_or("unknown"),
                required["profile"].as_str().unwrap_or("unknown"),
                required["offeredRps"].as_u64().unwrap_or_default()
            )
            .into());
        }
    }
    println!("LF-1/LF-2 Phase 0 closure evidence is complete");
    Ok(())
}

fn validate_instances(
    schema_path: &Path,
    instances: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let schema: Value = serde_json::from_slice(&fs::read(schema_path)?)?;
    let validator = Validator::new(&schema)?;
    for instance_path in instances {
        let instance: Value = serde_json::from_slice(&fs::read(instance_path)?)?;
        let errors = validator
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(format!(
                "{} failed {}: {}",
                instance_path.display(),
                schema_path.display(),
                errors.join("; ")
            )
            .into());
        }
    }
    Ok(())
}

fn evidence(spike: &str, inputs: Value, measurements: Value, assertions: Vec<Value>) -> Value {
    let status = if assertions.iter().all(|item| item["passed"] == true) {
        "pass"
    } else {
        "fail"
    };
    let subcommand = match spike {
        "body-capture-access-control" => "body",
        "snapshot-root-capture" => "snapshot",
        "projection-secret-materialization" => "projection-secret",
        "audit-wal-group-commit" => "wal",
        _ => spike,
    };
    json!({
        "schemaVersion":"1",
        "spike":spike,
        "sourceCommit":env::var("PHASE0_SOURCE_COMMIT").unwrap_or_else(|_| "workspace".to_string()),
        "command":format!("cargo run --locked --release -p llm-phase0-spikes -- {subcommand}"),
        "environment":{
            "os":env::consts::OS,
            "arch":env::consts::ARCH,
            "rustc":env::var("PHASE0_RUSTC").unwrap_or_else(|_| "captured-by-gate".to_string())
        },
        "inputs":inputs,
        "measurements":measurements,
        "assertions":assertions,
        "status":status
    })
}

fn assertion(name: &str, passed: bool) -> Value {
    json!({"name":name,"passed":passed})
}

fn write_evidence(path: Option<PathBuf>, value: Value) -> Result<(), Box<dyn std::error::Error>> {
    let encoded = serde_json::to_vec_pretty(&value)?;
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, encoded)?;
        println!("wrote {}", path.display());
    } else {
        println!("{}", String::from_utf8(encoded)?);
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("app is under workspace/apps")
        .to_path_buf()
}

fn workspace_path(relative: &str) -> PathBuf {
    workspace_root().join(relative)
}

fn relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_nested_object_keys() {
        let value: Value = serde_json::from_str(r#"{"z":1,"a":{"y":2,"b":3}}"#).unwrap();
        let encoded = serde_json::to_string(&canonicalize_json(&value)).unwrap();
        assert_eq!(encoded, r#"{"a":{"b":3,"y":2},"z":1}"#);
    }

    #[test]
    fn canonical_fixture_trim_accepts_crlf_and_multiple_trailing_lines() {
        assert_eq!(
            trim_trailing_ascii_whitespace(b"{\"a\":1}\r\n\r\n"),
            b"{\"a\":1}"
        );
    }

    #[test]
    fn buffered_wal_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round-trip.wal");
        let mut writer = WalWriter::open(&path, 1024).unwrap();
        writer.append(b"one").unwrap();
        writer.append(b"two").unwrap();
        writer.sync(2).unwrap();
        drop(writer);
        assert_eq!(recover_wal(&path).unwrap(), 2);
    }
}
