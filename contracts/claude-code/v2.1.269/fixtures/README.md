# Fixture provenance

`new-success.jsonl` is synthetic minimal parser input, shaped after the observed
CLI envelopes. Its marker, identifiers, and usage numbers are invented. It is
not a captured user conversation or proof of live execution. Failure cases are
constructed by `prototypes/claude-code-v1/test_phase0.py`.

The neighboring `phase0-live-evidence.json` records sanitized observations from
the live probe: event type/field names and allowlisted metadata only. It is not
an exhaustive protocol schema and does not qualify the production worker.
