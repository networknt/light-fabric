# LLM gateway operations

These runbooks are owned by `gateway-sre`. Start every incident by recording
the active public alias, publication digest, gateway instance, and UTC window.
Never copy prompts, completions, provider error bodies, credentials, principal
IDs, or request IDs into tickets or chat. Use the sanitized canary evidence
query and bounded metric labels.

A synthetic trigger must be exercised in a non-production environment after
changing an alert expression. Canary rollout remains blocked until PERF-3,
OBS-1, and SEC-1 are all closed by the release manifest.
