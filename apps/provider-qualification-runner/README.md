# Provider Qualification Runner

This separately deployed workload consumes bounded pending-work pages, probes
the exact published endpoint, signs `ConformanceResult` v2 with a protected
Ed25519 seed, and completes the exact optimistic deployment revision. It uses
separate clients and credentials for Portal control traffic and provider
traffic, disables environment proxies and redirects, and records only bounded
case identifiers and sanitized categories.

The first release must run one replica per host/environment. The Kubernetes
example uses `replicas: 1`, `Recreate`, and a process-local create-new lease;
horizontal replicas are unsupported until work claims are fenced by Portal.

Sidecar qualification must run from a namespace distinct from the target. The
task contract records both namespace identities and an isolation-manifest
digest. The LMT-G5 cluster exercise proves that the raw runtime target is
unreachable while the published sidecar remains reachable.
