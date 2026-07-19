# ADR 0006: MVP Accounting, Circuit, and Replay Defaults

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before LF-5B

## Decision

The initial measurable configuration keys are:

- `llm-router.accounting.estimatorId`, `estimatorVersion`,
  `safetyMarginBps`, `maxInputUnits`, `maxOutputUnits`,
  `maxReservedCostMicros`, and `unknownPricingMode`;
- `llm-router.circuit.failureThreshold=5`,
  `openCooldownMs=30000`, and `halfOpenProbePermits=1`;
- `llm-router.retry.maxAttempts=1` for the first buffered slice and
  `maxReplayBytes=1048576`.

Reservations are per replica and keyed by host/principal/alias; they are
explicitly non-distributed. The conservative local estimator is identified by
wire profile and is never reported as provider billing usage. Hard accounting
fails closed on unknown pricing; observational profiles preserve unknown and
incomplete evidence.

Timeout/cancellation reconciliation remains conservative. Passive circuits
count configured transport, timeout, throttling, and provider 5xx categories.
`Retry-After` cooldown belongs to provider-account/quota-group and deployment
state. Any future multi-attempt policy whose canonical replay body exceeds
`maxReplayBytes` is rejected at publication.

Numeric defaults remain manifest inputs and may change only with new benchmark
and pricing evidence.

