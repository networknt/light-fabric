# A2A Phase 7 Production Qualification

Phase 7 adds no protocol capability. It is the release boundary for the
governed A2A profiles implemented through Phase 6.

`qualification-contract.json` defines the evidence an operator must collect
for one concrete Host, environment, publication generation, gateway image, and
runtime image. Checked-in source and CI can prove the automated gates; they
cannot truthfully manufacture a production canary duration, alert review, or
rollback approval. A release therefore starts as `NOT_QUALIFIED` and becomes
`QUALIFIED` only in an external evidence document after every required check is
bound to immutable image and snapshot digests.

Production activation rules:

- canary one Host/environment and one publication generation;
- require both inbound and outbound probes;
- keep the previous valid publication generation as the rollback target;
- prove a second worker can reclaim an expired push lease and that the old
  owner cannot complete it;
- prove retry exhaustion reaches durable dead-letter state;
- use `/_a2a/ready`, not liveness, for traffic admission;
- reject stale or expired projections and never synthesize runtime authority
  from local deployment values; and
- stop rollout on cross-tenant access, signature, replay, destination, task
  ownership, or data-boundary failures.

The three supported development/install Compose profiles are contract checks,
not production evidence. They must stay byte-aligned with the canonical
`light-a2a` module templates and mount both operational and artifact stores.

