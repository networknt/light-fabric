# Provider conformance corpus

This directory contains the checked-in LF-4 codec regression corpus and its reproducible OpenAI and Anthropic conformance results. It is not evidence that the codecs were exercised against live provider endpoints.

The `v1/manifest.json` file pins every fixture by SHA-256 digest and records its provenance and the capabilities it covers. The current fixtures are all `synthetic_spec_derived`: they were hand-authored from the selected API contracts and were not captured from live provider traffic. A future sanitized capture must be marked `captured_sanitized` and still contain no credentials, customer prompts, or other PII.

Fixtures cover canonical request projection, multimodal and tool messages, structured output, optional usage fields, reasoning redaction, provider errors and `Retry-After`, arbitrary stream chunk boundaries, malformed events, and cross-provider compatibility rejection. Coverage tags are validated against the fixture structure: for example, `images` requires an image block, `structured_json` requires the corresponding response format, and `parallel_tools` requires at least two tool calls or stream indices. A capability is attested only when at least one structurally valid fixture tagged for that capability ran and passed. Removing its last covering fixture therefore removes that capability from the generated report.

Each report carries `capabilityEvidence` keyed by capability, with the passing fixture IDs and their provenance. This distinction is intentional: synthetic evidence proves codec behavior, not that a particular deployed model accepts the feature. `CapabilityRequirements.required_provenance` lets eligibility require `captured_sanitized` evidence centrally. The check applies to the requested operation and every requested image, tool, parallel-tool, structured-output, or streaming capability, so LF-5 cannot accidentally bypass it at an individual call site.

Provenance matching is existential: a capability satisfies `CapturedSanitized` when at least one passing covering fixture has that provenance. Additional synthetic fixtures do not invalidate genuine captured evidence, which supports incremental corpus migration. `reasoning_usage` is intentionally not a routing requirement because it describes optional accounting/observability metadata rather than a request feature. If routing or admission later depends on it, add a requirement flag and its provenance arm together in `CapabilityRequirements::required_attestations`.

Run the complete gate from the repository root:

```bash
./scripts/run-llm-provider-conformance-gates.sh
```

The gate runs `model-provider`, `light-agent`, and `light-workflow` tests, regenerates both provider reports at the fixed corpus timestamp, and compares them with `results/openai.json` and `results/anthropic.json`. Reports are self-digested, identify the codec/model/API/capability profile and canonical manifest digest, include `validUntil`, and contain fixture identifiers only.

The SHA-256 fields detect accidental corruption and bind a report to its corpus; they are not signatures and provide no authenticity against an actor who can rewrite both content and digest. Conformance results must travel inside the authenticated, authorized publication channel and versioned root-manifest contract. Adding HMAC or signatures here would require a separately approved signer identity, key distribution, rotation, and gateway verification contract.

Codec drift is deliberately strict in this phase. Unknown typed Anthropic blocks/events are protocol errors that fail conformance and trigger quarantine while the last valid deployment remains active. OpenAI success streams require the `[DONE]` terminal marker. These policies match the pinned-codec/quarantine design and must be changed through a versioned contract decision before LF-5 runtime wiring.

Provider reasoning content is deliberately excluded from `InferenceResponse`, legacy adapter responses, logs, and fixture results. Only normalized reasoning-token usage may be exposed. This is an intentional security contract, not an unimplemented migration field.

A provider is deployment-eligible only when its result exists, has not expired, passed every required case, provides the requested capabilities, and is not quarantined. Quarantine publication uses a monotonically increasing sequence and root digest; gateways acknowledge the exact sequence and root before convergence is declared.
