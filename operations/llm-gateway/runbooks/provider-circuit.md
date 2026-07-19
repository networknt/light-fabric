# Provider, circuit, and fallback failure

1. Break down attempts by bounded deployment/outcome labels and compare logical
   request failures with physical attempts and fallback depth.
2. Check rate-limit, authentication, overload, network, and protocol categories;
   never inspect or export raw provider bodies.
3. Half-open probes are replica-local. Expect at most one probe per replica,
   randomized by configured cooldown/jitter, and inspect
   `light_llm_circuit_probes_total` before declaring recovery.
4. If the provider cannot tolerate the bounded cross-replica probe burst, eject
   the deployment by publishing the prior routing manifest or disable it at the
   control plane. Do not increase retries during an outage.
5. Expand again only after probes succeed, fallback returns to baseline, and
   the provider confirms recovery.
