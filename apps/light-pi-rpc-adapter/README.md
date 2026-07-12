# light-pi-rpc-adapter

Trusted adapter for the pinned Pi RPC coding profile. The admitted
implementation is `@earendil-works/pi-coding-agent` `0.80.6`; both its reported
version and resolved executable SHA-256 digest are checked before launch.

The adapter materializes an immutable Git bundle, launches
`pi --mode rpc --no-session` with a cleared environment, enforces strict
bounded LF-delimited JSONL, requires correlated prompt acceptance and
`agent_settled`, rejects extension UI authority, and independently validates
the canonical patch and changed paths. The admitted `fs.read`/`fs.write` set is
translated to an explicit Pi `--tools read,write,edit`; Pi's default `bash`
tool is never enabled implicitly.

The Cube image must provide a read-only `/opt/light-pi/config` containing only
an approved broker-aware model definition. It must not contain a reusable
provider credential. Provider access is expected to use the runner-owned,
attempt-bound broker transport.

`cube_failure_matrix_live.rs` is the ignored live deployment gate for non-zero
exit, timeout, cancellation, lost client response, inspection, and idempotent
cleanup. It requires the documented `LIGHT_CUBE_TEST_*` configuration.
