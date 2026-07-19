use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    latency_micros: u64,
    status: u16,
    response_bytes: usize,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistogramSummary {
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    p999_micros: u64,
    max_micros: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkResult {
    schema_version: String,
    candidate: String,
    profile: String,
    target: String,
    payload_sha256: String,
    offered_rps: u64,
    duration_seconds: u64,
    max_inflight: usize,
    started_epoch_seconds: u64,
    offered: u64,
    admitted: u64,
    completed: u64,
    rejected_by_generator: u64,
    succeeded: u64,
    failed: u64,
    retried: u64,
    cancelled: u64,
    response_bytes: u64,
    generator_saturated: bool,
    latency: HistogramSummary,
    samples_micros: Vec<u64>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = required("LLM_BENCH_TARGET")?;
    let payload_path = required("LLM_BENCH_PAYLOAD")?;
    let output_path = required("LLM_BENCH_OUTPUT")?;
    let candidate = env::var("LLM_BENCH_CANDIDATE").unwrap_or_else(|_| "direct".to_string());
    let profile = env::var("LLM_BENCH_PROFILE").unwrap_or_else(|_| "stable-60ms".to_string());
    let offered_rps = parse("LLM_BENCH_RPS", 500_u64)?;
    let duration_seconds = parse("LLM_BENCH_DURATION_SECONDS", 30_u64)?;
    let max_inflight = parse("LLM_BENCH_MAX_INFLIGHT", 8192_usize)?;
    if offered_rps == 0 || duration_seconds == 0 || max_inflight == 0 {
        return Err("RPS, duration, and max inflight must be greater than zero".into());
    }
    let payload = Arc::new(fs::read(&payload_path)?);
    serde_json::from_slice::<Value>(&payload)?;
    let payload_sha256 = hex_sha256(&payload);
    let client = Client::builder()
        .pool_max_idle_per_host(max_inflight.min(4096))
        .tcp_nodelay(true)
        .build()?;
    let samples = Arc::new(Mutex::new(Vec::<Sample>::new()));
    let admission = Arc::new(Semaphore::new(max_inflight));
    let mut tasks = JoinSet::new();
    let tick = Duration::from_nanos(1_000_000_000_u64 / offered_rps);
    let mut ticker = interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let started_epoch_seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let deadline = Instant::now() + Duration::from_secs(duration_seconds);
    let mut offered = 0_u64;
    let mut admitted = 0_u64;
    let mut rejected_by_generator = 0_u64;
    while Instant::now() < deadline {
        ticker.tick().await;
        offered += 1;
        let Ok(permit) = Arc::clone(&admission).try_acquire_owned() else {
            rejected_by_generator += 1;
            continue;
        };
        admitted += 1;
        let client = client.clone();
        let target = target.clone();
        let payload = Arc::clone(&payload);
        let samples = Arc::clone(&samples);
        let profile = profile.clone();
        tasks.spawn(async move {
            let started = Instant::now();
            let response = client
                .post(target)
                .header("content-type", "application/json")
                .header("x-mock-profile", profile)
                .body(payload.as_ref().clone())
                .send()
                .await;
            let sample = match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    match response.bytes().await {
                        Ok(body) => Sample {
                            latency_micros: micros(started.elapsed()),
                            status,
                            response_bytes: body.len(),
                            error: None,
                        },
                        Err(error) => Sample {
                            latency_micros: micros(started.elapsed()),
                            status,
                            response_bytes: 0,
                            error: Some(error.to_string()),
                        },
                    }
                }
                Err(error) => Sample {
                    latency_micros: micros(started.elapsed()),
                    status: 0,
                    response_bytes: 0,
                    error: Some(error.to_string()),
                },
            };
            samples.lock().await.push(sample);
            drop(permit);
        });
    }
    while tasks.join_next().await.is_some() {}
    let samples = Arc::try_unwrap(samples)
        .map_err(|_| "benchmark sample references remain")?
        .into_inner();
    let mut latencies = samples
        .iter()
        .map(|sample| sample.latency_micros)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let succeeded = samples
        .iter()
        .filter(|sample| (200..300).contains(&sample.status) && sample.error.is_none())
        .count() as u64;
    let failed = samples.len() as u64 - succeeded;
    let result = BenchmarkResult {
        schema_version: "1".to_string(),
        candidate,
        profile,
        target,
        payload_sha256,
        offered_rps,
        duration_seconds,
        max_inflight,
        started_epoch_seconds,
        offered,
        admitted,
        completed: samples.len() as u64,
        rejected_by_generator,
        succeeded,
        failed,
        retried: 0,
        cancelled: 0,
        response_bytes: samples
            .iter()
            .map(|sample| sample.response_bytes as u64)
            .sum(),
        generator_saturated: rejected_by_generator > 0,
        latency: HistogramSummary {
            p50_micros: percentile(&latencies, 0.50),
            p95_micros: percentile(&latencies, 0.95),
            p99_micros: percentile(&latencies, 0.99),
            p999_micros: percentile(&latencies, 0.999),
            max_micros: latencies.last().copied().unwrap_or_default(),
        },
        samples_micros: latencies,
    };
    fs::write(&output_path, serde_json::to_vec_pretty(&result)?)?;
    println!("wrote {output_path}");
    if result.generator_saturated {
        return Err("load generator saturated; benchmark is invalid".into());
    }
    Ok(())
}

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("missing {name}").into())
}

fn parse<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + 'static,
{
    env::var(name)
        .map(|value| value.parse::<T>())
        .unwrap_or(Ok(default))
        .map_err(Into::into)
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}

fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
