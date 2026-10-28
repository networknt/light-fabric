# Partial X-04 owned local profile

The handler/logging files are the activated supported-input fixture used by the
new built-app test. Values and synthetic issuer/audience/JWKS credentials are
supplied by the existing L1 harness; no private key, token or credential is saved
here. The trusted process gate is `LIGHT_LAMBDA_ALLOW_LOGGER_SET=1`, startup
`RUST_LOG=info`, and `logger.enabled=true` (or the explicit logging.yml here).
Default gate absence/0 remains closed. Only ROOT INFO/DEBUG/TRACE commands within
1024 bytes,0–1 directives,no unknown fields are admitted.

This fixture intentionally uses `/qualification/logger`. The native sample's
POST `/adm/logger` is still rejected by two unchanged reservation guards. Their
separate HIGH-impact patch is proposed, not applied. This profile executes the
real setter factory, Init, subscriber, JWT verification and service path; it is
not native reserved-route, full sample, package/RIE or AWS qualification.

From the worktree, build and rerun all focused qualification in fresh evidence:

```bash
rtk proxy env CARGO_INCREMENTAL=0 WP10_L1_EVIDENCE_DIR=/home/steve/workspace/artifacts/implementation/light-lambda/logger-set-rerun-$(date -u +%Y%m%dT%H%M%SZ) cargo test --locked --offline -p light-lambda --test l1 logger_set:: -- --test-threads=1
```

The subprocess-only helper remains ignored in the ordinary suite and is launched
explicitly by the concurrency/reset parent test; it is a normal application test
process, not a precision worker. Validator acceptance/paused experiments do not
run. The delivered ZIP is not replaced; these tests build a separate debug
bootstrap and record its identity at the delivery checkpoint.
