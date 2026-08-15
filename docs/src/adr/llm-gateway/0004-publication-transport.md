# ADR 0004: LLM Configuration Uses the Standard Config Lifecycle

- Status: Superseded filesystem projection; accepted values-backed lifecycle
- Date: 2026-08-15

## Decision

`llm-router` uses the same configuration authority and lifecycle as every
other reloadable gateway module. The config server's current immutable
`values.yml` snapshot is the only source of LLM routing configuration.

At startup, `LightRuntime` downloads the current snapshot, resolves
`llm-router.yml`, compiles the complete providers/deployments/aliases graph,
and publishes one immutable runtime snapshot. During an explicit module reload,
the runtime downloads the current snapshot again and invokes only the selected
reloaders. `LlmRouterReloader` compiles from that fresh reload context and
atomically swaps the candidate only after validation succeeds. A failed reload
retains the last-known-good LLM runtime.

The former config-server `/files` manifest/resource projection,
`LlmProjectionWorker` polling loop, projection checkpoint, and gateway-to-Portal
publication acknowledgement are removed. LLM configuration cannot change merely
because files appeared in `config-cache`; it changes only at startup or when
`llm-router` is included in an explicit reload.

## Configuration Boundary

The typed `llm-router.*` properties in `values.yml` include the complete
provider, deployment, alias, policy-derived, pricing, and non-secret runtime
material configuration. Map and list properties are whole typed nodes, not
quoted JSON strings.

Credential and reasoning-seal values remain outside config server. The snapshot
contains only `env:` or opaque credential references plus the authorized local
reference-to-environment mapping. Trust-bundle configuration similarly contains
only approved references, paths, and digests; it does not contain private key or
provider credential bytes.

The config snapshot is immutable. Publishing control-plane changes means
updating the target instance's `llm-router` properties, creating/promoting a new
snapshot through the normal config workflow, and then explicitly restarting or
reloading `llm-router`.

## Consequences

- Startup and reload observe one coherent config-server snapshot.
- Reloading an unrelated module cannot alter LLM routing.
- In-flight requests retain the immutable runtime snapshot they captured.
- There is no second polling, sequence, checkpoint, or acknowledgement protocol.
- Delivery success is reported by the standard module reload result; provider
  reachability remains a separate runtime qualification concern.
