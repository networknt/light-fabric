use codex_core::{StartThreadOptions, ThreadManager};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::time::Instant;

const UPSTREAM_VERSION: &str = "0.153.2";
const UPSTREAM_REVISION: &str = "657a993cbee87acf52d14b758ce49dbd46d1b8eb";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundaryRequest {
    id: u64,
    method: String,
    cwd: String,
    prompt: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations = std::env::args()
        .nth(1)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(100_000);
    if iterations == 0 || iterations > 10_000_000 {
        return Err("iterations must be between 1 and 10000000".into());
    }

    // Referencing both exported types makes upstream API removal a compile
    // failure without starting Codex or discovering ambient credentials.
    let linked_type_bytes = std::mem::size_of::<ThreadManager>()
        .saturating_add(std::mem::size_of::<StartThreadOptions>());
    let request = BoundaryRequest {
        id: 1,
        method: "turn/start".into(),
        cwd: "/workspace/repository".into(),
        prompt: "validate the candidate patch".into(),
    };

    let direct_started = Instant::now();
    for _ in 0..iterations {
        black_box(&request);
    }
    let direct_nanos = direct_started.elapsed().as_nanos();

    let json_started = Instant::now();
    for _ in 0..iterations {
        let encoded = serde_json::to_vec(black_box(&request))?;
        let decoded: BoundaryRequest = serde_json::from_slice(black_box(&encoded))?;
        black_box(decoded);
    }
    let json_nanos = json_started.elapsed().as_nanos();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schemaVersion": 1,
            "adapterId": "codex-embedded-v1",
            "status": "prototype-only",
            "upstreamVersion": UPSTREAM_VERSION,
            "upstreamRevision": UPSTREAM_REVISION,
            "linkedPublicTypes": ["codex_core::ThreadManager", "codex_core::StartThreadOptions"],
            "linkedTypeBytes": linked_type_bytes,
            "benchmark": {
                "scope": "typed-call-versus-json-boundary-only",
                "iterations": iterations,
                "directNanosecondsTotal": direct_nanos,
                "jsonNanosecondsTotal": json_nanos
            },
            "productionQualified": false
        }))?
    );
    Ok(())
}
