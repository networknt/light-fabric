# Audit and WAL pressure

1. Check reservation failures, WAL bytes/segments, durable watermark,
   acknowledged sequence, sink lag, and database health.
2. Required audit modes fail closed. Do not switch an alias to best effort to
   restore traffic during an incident.
3. Restore the separately credentialed audit sink, then verify idempotent replay
   advances the local checkpoint before inactive segment reclamation.
4. For corruption, stop the writer, preserve the directory, and recover only a
   torn final tail. Corruption in a completed segment requires escalation.
5. Local-durable storage must preserve `flock`, `fdatasync`, atomic rename, and
   crash-safe directory entries; unverified network storage is not acceptable.
