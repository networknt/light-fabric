use super::{AuditEvent, AuditEventKind};
use crate::error::LlmGatewayError;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"LLMAUD01";
const FORMAT_VERSION: u16 = 1;
const GATEWAY_BYTES: usize = 64;
pub(super) const HEADER_BYTES: u64 = (8 + 2 + 16 + GATEWAY_BYTES + 8) as u64;
const RECORD_PREFIX_BYTES: u64 = 4 + 32 + 8;

#[derive(Debug, Clone)]
pub struct WalConfig {
    pub directory: PathBuf,
    pub gateway_instance: String,
    pub max_record_bytes: usize,
    pub max_segment_bytes: u64,
    pub max_spool_bytes: u64,
    pub queue_records: usize,
    pub batch_records: usize,
    pub batch_bytes: usize,
    pub commit_delay: Duration,
    pub terminal_commit_before_response: bool,
    pub persistent_volume: bool,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("data/llm-audit"),
            gateway_instance: "gateway-local".to_string(),
            max_record_bytes: 4 * 1024,
            max_segment_bytes: 64 * 1024 * 1024,
            max_spool_bytes: 1024 * 1024 * 1024,
            queue_records: 8_192,
            batch_records: 64,
            batch_bytes: 256 * 1024,
            commit_delay: Duration::from_millis(5),
            terminal_commit_before_response: false,
            persistent_volume: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalStatus {
    pub durable_sequence: u64,
    pub acknowledged_sequence: u64,
    pub sink_lag_records: u64,
    pub wal_bytes: u64,
    pub segments: u64,
    pub writer_failed: bool,
    pub recovered_incomplete_attempts: u64,
}

struct AppendCommand {
    sequence: u64,
    payload: Vec<u8>,
    durable: bool,
    acknowledgement: Option<oneshot::Sender<Result<u64, String>>>,
}

pub struct AuditWal {
    pub(super) config: WalConfig,
    sender: mpsc::SyncSender<AppendCommand>,
    next_sequence: AtomicU64,
    durable_sequence: Arc<AtomicU64>,
    acknowledged_sequence: AtomicU64,
    wal_bytes: Arc<AtomicU64>,
    segments: Arc<AtomicU64>,
    writer_failed: Arc<AtomicBool>,
    reserved_bytes: AtomicU64,
    append_lock: Mutex<()>,
    io_lock: Arc<Mutex<()>>,
    reclaimed_bytes: Arc<AtomicU64>,
    recovered_incomplete_attempts: u64,
}

impl AuditWal {
    pub fn open(config: WalConfig) -> Result<Arc<Self>, LlmGatewayError> {
        validate_config(&config)?;
        fs::create_dir_all(&config.directory).map_err(config_error)?;
        let directory_lock = acquire_directory_lock(&config.directory)?;
        let recovered = recover(&config)?;
        let acknowledged_sequence = load_checkpoint(&config.directory)?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_records);
        let durable_sequence = Arc::new(AtomicU64::new(recovered.last_sequence));
        let wal_bytes = Arc::new(AtomicU64::new(recovered.bytes));
        let segments = Arc::new(AtomicU64::new(recovered.segments));
        let writer_failed = Arc::new(AtomicBool::new(false));
        let io_lock = Arc::new(Mutex::new(()));
        let reclaimed_bytes = Arc::new(AtomicU64::new(0));
        spawn_writer(
            config.clone(),
            receiver,
            Arc::clone(&durable_sequence),
            Arc::clone(&wal_bytes),
            Arc::clone(&segments),
            Arc::clone(&writer_failed),
            Arc::clone(&io_lock),
            Arc::clone(&reclaimed_bytes),
            directory_lock,
        )?;
        Ok(Arc::new(Self {
            config,
            sender,
            next_sequence: AtomicU64::new(recovered.last_sequence),
            durable_sequence,
            acknowledged_sequence: AtomicU64::new(acknowledged_sequence),
            wal_bytes,
            segments,
            writer_failed,
            reserved_bytes: AtomicU64::new(0),
            append_lock: Mutex::new(()),
            io_lock,
            reclaimed_bytes,
            recovered_incomplete_attempts: recovered.incomplete_attempts,
        }))
    }

    pub fn reserve_envelope(
        self: &Arc<Self>,
        max_attempts: usize,
    ) -> Result<WalEnvelope, LlmGatewayError> {
        if self.writer_failed.load(Ordering::Acquire) {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        let records = 2_usize.saturating_add(max_attempts.saturating_mul(2));
        let bytes = records
            .checked_mul(
                self.config
                    .max_record_bytes
                    .saturating_add(RECORD_PREFIX_BYTES as usize)
                    .saturating_add(HEADER_BYTES as usize),
            )
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(LlmGatewayError::AuditUnavailable)?;
        let mut reserved = self.reserved_bytes.load(Ordering::Acquire);
        loop {
            let projected = self
                .wal_bytes
                .load(Ordering::Acquire)
                .saturating_add(reserved)
                .saturating_add(bytes);
            if projected > self.config.max_spool_bytes {
                return Err(LlmGatewayError::AuditUnavailable);
            }
            match self.reserved_bytes.compare_exchange_weak(
                reserved,
                reserved.saturating_add(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(WalEnvelope {
                        wal: Arc::clone(self),
                        bytes,
                    });
                }
                Err(current) => reserved = current,
            }
        }
    }

    pub async fn append(&self, event: &AuditEvent, durable: bool) -> Result<u64, LlmGatewayError> {
        if self.writer_failed.load(Ordering::Acquire) {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        let payload = serde_json::to_vec(event).map_err(|_| LlmGatewayError::AuditUnavailable)?;
        if payload.len() > self.config.max_record_bytes {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        let (acknowledgement, receiver) = if durable {
            let (sender, receiver) = oneshot::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let sequence = {
            let _guard = self
                .append_lock
                .lock()
                .map_err(|_| LlmGatewayError::AuditUnavailable)?;
            let sequence = self.next_sequence.load(Ordering::Acquire).saturating_add(1);
            self.sender
                .try_send(AppendCommand {
                    sequence,
                    payload,
                    durable,
                    acknowledgement,
                })
                .map_err(|_| LlmGatewayError::AuditUnavailable)?;
            self.next_sequence.store(sequence, Ordering::Release);
            sequence
        };
        if let Some(receiver) = receiver {
            receiver
                .await
                .map_err(|_| LlmGatewayError::AuditUnavailable)?
                .map_err(|_| LlmGatewayError::AuditUnavailable)
        } else {
            Ok(sequence)
        }
    }

    pub fn terminal_commit_before_response(&self) -> bool {
        self.config.terminal_commit_before_response
    }

    pub fn status(&self) -> WalStatus {
        let durable_sequence = self.durable_sequence.load(Ordering::Acquire);
        let acknowledged_sequence = self.acknowledged_sequence.load(Ordering::Acquire);
        WalStatus {
            durable_sequence,
            acknowledged_sequence,
            sink_lag_records: durable_sequence.saturating_sub(acknowledged_sequence),
            wal_bytes: self.wal_bytes.load(Ordering::Acquire),
            segments: self.segments.load(Ordering::Acquire),
            writer_failed: self.writer_failed.load(Ordering::Acquire),
            recovered_incomplete_attempts: self.recovered_incomplete_attempts,
        }
    }

    pub fn replay_batch(
        &self,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<Vec<WalRecord>, LlmGatewayError> {
        if max_records == 0 || max_bytes == 0 {
            return Ok(Vec::new());
        }
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| LlmGatewayError::AuditUnavailable)?;
        read_records(
            &self.config,
            self.acknowledged_sequence.load(Ordering::Acquire),
            max_records,
            max_bytes,
        )
    }

    /// Persist the authoritative sink checkpoint before reclaiming fully
    /// acknowledged inactive segments.
    pub fn acknowledge(&self, sequence: u64) -> Result<(), LlmGatewayError> {
        let durable = self.durable_sequence.load(Ordering::Acquire);
        let current = self.acknowledged_sequence.load(Ordering::Acquire);
        if sequence < current || sequence > durable {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| LlmGatewayError::AuditUnavailable)?;
        persist_checkpoint(&self.config.directory, sequence)?;
        self.acknowledged_sequence
            .store(sequence, Ordering::Release);
        reclaim_segments(
            &self.config,
            sequence,
            &self.wal_bytes,
            &self.segments,
            &self.reclaimed_bytes,
        )
    }
}

fn acquire_directory_lock(directory: &Path) -> Result<File, LlmGatewayError> {
    let lock_path = directory.join("writer.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(config_error)?;
    lock.try_lock().map_err(|error| {
        LlmGatewayError::Config(format!(
            "LLM audit WAL directory already has an active writer: {error}"
        ))
    })?;
    Ok(lock)
}

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub sequence: u64,
    pub event: AuditEvent,
    pub encoded_bytes: usize,
}

pub struct WalEnvelope {
    wal: Arc<AuditWal>,
    bytes: u64,
}

impl Drop for WalEnvelope {
    fn drop(&mut self) {
        self.wal
            .reserved_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

fn validate_config(config: &WalConfig) -> Result<(), LlmGatewayError> {
    if config.gateway_instance.is_empty()
        || config.gateway_instance.len() > GATEWAY_BYTES
        || config.max_record_bytes == 0
        || config.max_segment_bytes < HEADER_BYTES + RECORD_PREFIX_BYTES + 1
        || config.max_spool_bytes < config.max_segment_bytes
        || config.queue_records == 0
        || config.batch_records == 0
        || config.batch_bytes < config.max_record_bytes
        || config.commit_delay.is_zero()
    {
        return Err(LlmGatewayError::Config(
            "invalid LLM audit WAL bounds".to_string(),
        ));
    }
    Ok(())
}

struct Recovery {
    last_sequence: u64,
    bytes: u64,
    segments: u64,
    incomplete_attempts: u64,
}

fn recover(config: &WalConfig) -> Result<Recovery, LlmGatewayError> {
    let mut paths = wal_paths(&config.directory)?;
    paths.sort();
    let mut last_sequence = 0_u64;
    let mut bytes = 0_u64;
    let mut active_attempts = std::collections::BTreeSet::new();
    let path_count = paths.len();
    for (index, path) in paths.iter().enumerate() {
        let events = recover_segment(
            path,
            index + 1 == path_count,
            config.max_record_bytes,
            &config.gateway_instance,
            &mut last_sequence,
        )?;
        bytes = bytes.saturating_add(fs::metadata(path).map_err(config_error)?.len());
        for event in events {
            let key = (event.request_id, event.attempt.unwrap_or_default());
            match event.kind {
                AuditEventKind::AttemptStarted => {
                    active_attempts.insert(key);
                }
                AuditEventKind::AttemptFinished => {
                    active_attempts.remove(&key);
                }
                _ => {}
            }
        }
    }
    Ok(Recovery {
        last_sequence,
        bytes,
        segments: paths.len() as u64,
        incomplete_attempts: active_attempts.len() as u64,
    })
}

fn read_records(
    config: &WalConfig,
    after: u64,
    max_records: usize,
    max_bytes: usize,
) -> Result<Vec<WalRecord>, LlmGatewayError> {
    let mut paths = wal_paths(&config.directory)?;
    paths.sort();
    let mut records = Vec::new();
    let mut bytes = 0_usize;
    let mut prior_sequence = 0_u64;
    for path in paths {
        let mut file = File::open(path).map_err(config_error)?;
        verify_header(&mut file, &config.gateway_instance)?;
        while let Some((sequence, payload)) = read_record(&mut file, config.max_record_bytes)? {
            if sequence <= prior_sequence {
                return Err(corruption());
            }
            prior_sequence = sequence;
            if sequence <= after {
                continue;
            }
            let encoded_bytes = RECORD_PREFIX_BYTES as usize + payload.len();
            if !records.is_empty()
                && (records.len() >= max_records || bytes.saturating_add(encoded_bytes) > max_bytes)
            {
                return Ok(records);
            }
            let event = serde_json::from_slice(&payload).map_err(|_| corruption())?;
            bytes = bytes.saturating_add(encoded_bytes);
            records.push(WalRecord {
                sequence,
                event,
                encoded_bytes,
            });
            if records.len() >= max_records || bytes >= max_bytes {
                return Ok(records);
            }
        }
    }
    Ok(records)
}

fn read_record(
    file: &mut File,
    max_record_bytes: usize,
) -> Result<Option<(u64, Vec<u8>)>, LlmGatewayError> {
    let mut length_bytes = [0_u8; 4];
    match read_exact_or_eof(file, &mut length_bytes).map_err(config_error)? {
        0 => return Ok(None),
        4 => {}
        _ => return Ok(None),
    }
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 || length > max_record_bytes {
        return Err(corruption());
    }
    let mut checksum = [0_u8; 32];
    let mut sequence_bytes = [0_u8; 8];
    let mut payload = vec![0_u8; length];
    if file.read_exact(&mut checksum).is_err()
        || file.read_exact(&mut sequence_bytes).is_err()
        || file.read_exact(&mut payload).is_err()
    {
        // The active writer may not have completed its tail yet.
        return Ok(None);
    }
    let sequence = u64::from_be_bytes(sequence_bytes);
    if record_checksum(sequence, &payload).as_slice() != checksum {
        return Err(corruption());
    }
    Ok(Some((sequence, payload)))
}

fn checkpoint_path(directory: &Path) -> PathBuf {
    directory.join("sink-checkpoint.json")
}

fn load_checkpoint(directory: &Path) -> Result<u64, LlmGatewayError> {
    let path = checkpoint_path(directory);
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("acknowledgedSequence")?.as_u64())
            .ok_or_else(corruption),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(config_error(error)),
    }
}

fn persist_checkpoint(directory: &Path, sequence: u64) -> Result<(), LlmGatewayError> {
    let target = checkpoint_path(directory);
    let temporary = directory.join(format!(".sink-checkpoint-{}.tmp", Uuid::now_v7()));
    let payload = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "acknowledgedSequence": sequence
    }))
    .map_err(|_| LlmGatewayError::AuditUnavailable)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(config_error)?;
    file.write_all(&payload).map_err(config_error)?;
    file.sync_data().map_err(config_error)?;
    fs::rename(&temporary, &target).map_err(config_error)?;
    File::open(directory)
        .and_then(|directory| directory.sync_data())
        .map_err(config_error)
}

fn reclaim_segments(
    config: &WalConfig,
    acknowledged: u64,
    wal_bytes: &AtomicU64,
    segments: &AtomicU64,
    reclaimed_bytes: &AtomicU64,
) -> Result<(), LlmGatewayError> {
    let mut paths = wal_paths(&config.directory)?;
    paths.sort();
    paths.pop(); // the newest segment belongs to the active writer
    let mut reclaimed = 0_u64;
    for path in paths {
        let records =
            read_segment_records(&path, config.max_record_bytes, &config.gateway_instance)?;
        if records
            .last()
            .is_some_and(|record| record.sequence <= acknowledged)
        {
            reclaimed = reclaimed.saturating_add(fs::metadata(&path).map_err(config_error)?.len());
            fs::remove_file(path).map_err(config_error)?;
        }
    }
    let remaining = wal_paths(&config.directory)?;
    let bytes = remaining
        .iter()
        .try_fold(0_u64, |total, path| {
            fs::metadata(path).map(|metadata| total.saturating_add(metadata.len()))
        })
        .map_err(config_error)?;
    wal_bytes.store(bytes, Ordering::Release);
    segments.store(remaining.len() as u64, Ordering::Release);
    reclaimed_bytes.fetch_add(reclaimed, Ordering::AcqRel);
    Ok(())
}

fn read_segment_records(
    path: &Path,
    max_record_bytes: usize,
    gateway_instance: &str,
) -> Result<Vec<WalRecord>, LlmGatewayError> {
    let mut file = File::open(path).map_err(config_error)?;
    verify_header(&mut file, gateway_instance)?;
    let mut records = Vec::new();
    while let Some((sequence, payload)) = read_record(&mut file, max_record_bytes)? {
        let encoded_bytes = RECORD_PREFIX_BYTES as usize + payload.len();
        records.push(WalRecord {
            sequence,
            event: serde_json::from_slice(&payload).map_err(|_| corruption())?,
            encoded_bytes,
        });
    }
    Ok(records)
}

fn wal_paths(directory: &Path) -> Result<Vec<PathBuf>, LlmGatewayError> {
    Ok(fs::read_dir(directory)
        .map_err(config_error)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|value| value == "wal"))
        .collect())
}

fn recover_segment(
    path: &Path,
    last: bool,
    max_record_bytes: usize,
    gateway_instance: &str,
    last_sequence: &mut u64,
) -> Result<Vec<AuditEvent>, LlmGatewayError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(last)
        .open(path)
        .map_err(config_error)?;
    verify_header(&mut file, gateway_instance)?;
    let mut events = Vec::new();
    let mut offset = HEADER_BYTES;
    loop {
        let record_start = offset;
        let mut length_bytes = [0_u8; 4];
        match read_exact_or_eof(&mut file, &mut length_bytes).map_err(config_error)? {
            0 => break,
            4 => {}
            _ if last => {
                file.set_len(record_start).map_err(config_error)?;
                file.sync_data().map_err(config_error)?;
                break;
            }
            _ => return Err(corruption()),
        }
        let length = u32::from_be_bytes(length_bytes) as usize;
        if length == 0 || length > max_record_bytes {
            return Err(corruption());
        }
        let mut checksum = [0_u8; 32];
        let mut sequence_bytes = [0_u8; 8];
        let mut payload = vec![0_u8; length];
        let tail_result = file
            .read_exact(&mut checksum)
            .and_then(|_| file.read_exact(&mut sequence_bytes))
            .and_then(|_| file.read_exact(&mut payload));
        if let Err(error) = tail_result {
            if last && error.kind() == ErrorKind::UnexpectedEof {
                file.set_len(record_start).map_err(config_error)?;
                file.sync_data().map_err(config_error)?;
                break;
            }
            return Err(corruption());
        }
        let sequence = u64::from_be_bytes(sequence_bytes);
        if sequence <= *last_sequence {
            return Err(corruption());
        }
        let expected = record_checksum(sequence, &payload);
        if expected.as_slice() != checksum {
            return Err(corruption());
        }
        let event: AuditEvent = serde_json::from_slice(&payload).map_err(|_| corruption())?;
        events.push(event);
        *last_sequence = sequence;
        offset = offset.saturating_add(RECORD_PREFIX_BYTES + length as u64);
    }
    Ok(events)
}

fn verify_header(file: &mut File, gateway_instance: &str) -> Result<(), LlmGatewayError> {
    let mut header = vec![0_u8; HEADER_BYTES as usize];
    file.read_exact(&mut header).map_err(|_| corruption())?;
    if &header[..8] != MAGIC || u16::from_be_bytes([header[8], header[9]]) != FORMAT_VERSION {
        return Err(corruption());
    }
    let gateway_start = 8 + 2 + 16;
    let gateway_end = gateway_start + GATEWAY_BYTES;
    let stored_gateway = &header[gateway_start..gateway_end];
    let stored_len = stored_gateway
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(stored_gateway.len());
    if &stored_gateway[..stored_len] != gateway_instance.as_bytes() {
        return Err(LlmGatewayError::Config(
            "LLM audit WAL belongs to a different gateway instance".to_string(),
        ));
    }
    Ok(())
}

fn read_exact_or_eof(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..])? {
            0 => break,
            count => read += count,
        }
    }
    Ok(read)
}

#[allow(clippy::too_many_arguments)]
fn spawn_writer(
    config: WalConfig,
    receiver: mpsc::Receiver<AppendCommand>,
    durable_sequence: Arc<AtomicU64>,
    wal_bytes: Arc<AtomicU64>,
    segments: Arc<AtomicU64>,
    writer_failed: Arc<AtomicBool>,
    io_lock: Arc<Mutex<()>>,
    reclaimed_bytes: Arc<AtomicU64>,
    directory_lock: File,
) -> Result<(), LlmGatewayError> {
    let mut writer = SegmentWriter::open(&config)?;
    std::thread::Builder::new()
        .name("llm-audit-wal".to_string())
        .spawn(move || {
            // The OS advisory lock is deliberately owned by the writer
            // thread, not AuditWal. It therefore remains held while queued
            // non-durable records drain after the last sender is dropped.
            let _directory_lock = directory_lock;
            while let Ok(first) = receiver.recv() {
                let mut batch = vec![first];
                let mut bytes = batch[0].payload.len();
                let started = std::time::Instant::now();
                while batch.len() < config.batch_records && bytes < config.batch_bytes {
                    let remaining = config.commit_delay.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        break;
                    }
                    match receiver.recv_timeout(remaining) {
                        Ok(command) => {
                            bytes = bytes.saturating_add(command.payload.len());
                            batch.push(command);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let result = io_lock
                    .lock()
                    .map_err(|_| std::io::Error::other("audit WAL I/O lock poisoned"))
                    .and_then(|_guard| {
                        writer.total_bytes = writer
                            .total_bytes
                            .saturating_sub(reclaimed_bytes.swap(0, Ordering::AcqRel));
                        writer.append_batch(&batch).and_then(|sequence| {
                            writer.file.sync_data()?;
                            durable_sequence.store(sequence, Ordering::Release);
                            wal_bytes.store(writer.total_bytes, Ordering::Release);
                            segments.store(writer.segment_count, Ordering::Release);
                            Ok(sequence)
                        })
                    });
                if result.is_err() {
                    writer_failed.store(true, Ordering::Release);
                }
                for command in batch {
                    if command.durable
                        && let Some(sender) = command.acknowledgement
                    {
                        let _ = sender
                            .send(result.as_ref().copied().map_err(|error| error.to_string()));
                    }
                }
                if result.is_err() {
                    break;
                }
            }
        })
        .map_err(config_error)?;
    Ok(())
}

struct SegmentWriter {
    config: WalConfig,
    file: File,
    current_bytes: u64,
    total_bytes: u64,
    segment_count: u64,
}

impl SegmentWriter {
    fn open(config: &WalConfig) -> Result<Self, LlmGatewayError> {
        let existing = fs::read_dir(&config.directory)
            .map_err(config_error)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "wal"))
            .collect::<Vec<_>>();
        let existing_bytes: u64 = existing
            .iter()
            .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
            .sum();
        let (file, current_bytes) = create_segment(config)?;
        Ok(Self {
            config: config.clone(),
            file,
            current_bytes,
            total_bytes: existing_bytes.saturating_add(current_bytes),
            segment_count: existing.len() as u64 + 1,
        })
    }

    fn append_batch(&mut self, batch: &[AppendCommand]) -> std::io::Result<u64> {
        let mut last_sequence = 0;
        for command in batch {
            let record_bytes = RECORD_PREFIX_BYTES + command.payload.len() as u64;
            if self.current_bytes.saturating_add(record_bytes) > self.config.max_segment_bytes {
                let (file, bytes) = create_segment(&self.config).map_err(std::io::Error::other)?;
                self.file = file;
                self.current_bytes = bytes;
                self.total_bytes = self.total_bytes.saturating_add(bytes);
                self.segment_count = self.segment_count.saturating_add(1);
            }
            if self.total_bytes.saturating_add(record_bytes) > self.config.max_spool_bytes {
                return Err(std::io::Error::new(
                    ErrorKind::StorageFull,
                    "audit WAL full",
                ));
            }
            let checksum = record_checksum(command.sequence, &command.payload);
            let mut record = Vec::with_capacity(record_bytes as usize);
            record.extend_from_slice(&(command.payload.len() as u32).to_be_bytes());
            record.extend_from_slice(&checksum);
            record.extend_from_slice(&command.sequence.to_be_bytes());
            record.extend_from_slice(&command.payload);
            self.file.write_all(&record)?;
            self.current_bytes = self.current_bytes.saturating_add(record_bytes);
            self.total_bytes = self.total_bytes.saturating_add(record_bytes);
            last_sequence = command.sequence;
        }
        Ok(last_sequence)
    }
}

fn create_segment(config: &WalConfig) -> Result<(File, u64), LlmGatewayError> {
    let segment_id = Uuid::now_v7();
    let path = config.directory.join(format!("segment-{segment_id}.wal"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(config_error)?;
    let mut header = Vec::with_capacity(HEADER_BYTES as usize);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    header.extend_from_slice(segment_id.as_bytes());
    let mut gateway = [0_u8; GATEWAY_BYTES];
    gateway[..config.gateway_instance.len()].copy_from_slice(config.gateway_instance.as_bytes());
    header.extend_from_slice(&gateway);
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    header.extend_from_slice(&created.to_be_bytes());
    file.write_all(&header).map_err(config_error)?;
    file.sync_data().map_err(config_error)?;
    Ok((file, HEADER_BYTES))
}

fn record_checksum(sequence: u64, payload: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(sequence.to_be_bytes());
    digest.update(payload);
    digest.finalize().into()
}

fn corruption() -> LlmGatewayError {
    LlmGatewayError::Config("LLM audit WAL corruption detected".to_string())
}

fn config_error(error: impl std::fmt::Display) -> LlmGatewayError {
    LlmGatewayError::Config(format!("LLM audit WAL unavailable: {error}"))
}
