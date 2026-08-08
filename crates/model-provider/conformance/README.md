# Provider conformance corpus

This directory contains the checked-in codec regression corpus and reproducible results for OpenAI Chat, OpenAI Responses, OpenAI Embeddings, and Anthropic Messages. It is not evidence that the codecs were exercised against live provider endpoints.

The `v1/manifest.json` file pins every fixture by SHA-256 digest and records its provenance and the capabilities it covers. The current fixtures are all `synthetic_spec_derived`: they were hand-authored from the selected API contracts and were not captured from live provider traffic. A future sanitized capture must be marked `captured_sanitized` and still contain no credentials, customer prompts, or other PII.

Fixtures cover canonical request projection, multimodal and tool messages,
structured output, typed Responses items, refusals, reasoning summaries,
float/base64 embedding vectors, malformed embedding dimensions/encodings,
optional usage fields, provider errors and `Retry-After`, arbitrary stream chunk
boundaries, unknown events, and cross-provider compatibility rejection. Coverage
tags are validated against fixture structure. A capability is attested only when
at least one structurally valid fixture tagged for that capability ran and
passed. Removing its last covering fixture therefore removes that capability
from the generated report.

Each report carries `capabilityEvidence` keyed by capability, with the passing fixture IDs and their provenance. This distinction is intentional: synthetic evidence proves codec behavior, not that a particular deployed model accepts the feature. `CapabilityRequirements.required_provenance` lets eligibility require `captured_sanitized` evidence centrally. The check applies to the requested operation and every requested image, tool, parallel-tool, structured-output, or streaming capability, so LF-5 cannot accidentally bypass it at an individual call site.

Provenance matching is existential: a capability satisfies `CapturedSanitized`
when at least one passing covering fixture has that provenance. Additional
synthetic fixtures do not invalidate genuine captured evidence, which supports
incremental corpus migration. Ordinary requests do not require
`reasoning_usage`; an explicit Responses reasoning request does, and is eligible
only for an OpenAI Responses deployment with matching evidence.

Run the complete gate from the repository root:

```bash
./scripts/run-llm-provider-conformance-gates.sh
```

The gate runs `model-provider`, `light-agent`, and `light-workflow` tests, regenerates all four provider-protocol reports at the fixed corpus timestamp, and compares them with the files in `results/`. Reports are self-digested, identify the provider protocol/model/API/capability profile and canonical manifest digest, include `validUntil`, and contain fixture identifiers only.

The SHA-256 fields detect accidental corruption and bind a report to its corpus; they are not signatures and provide no authenticity against an actor who can rewrite both content and digest. Conformance results must travel inside the authenticated, authorized publication channel and versioned root-manifest contract. Adding HMAC or signatures here would require a separately approved signer identity, key distribution, rotation, and gateway verification contract.

Codec drift is deliberately strict. Unknown typed blocks/events are protocol errors that fail conformance and trigger quarantine while the last valid deployment remains active. OpenAI Chat success streams require `[DONE]`; OpenAI Responses streams require a typed terminal response event and must not contain `[DONE]`.

Private chain-of-thought is deliberately excluded from `InferenceResponse`,
legacy adapter responses, logs, and fixture results. Provider-authored public
reasoning summaries are retained as typed summary items when the route is
explicitly conformant; normalized reasoning-token usage remains separate.

A provider is deployment-eligible only when its result exists, has not expired, passed every required case, provides the requested capabilities, and is not quarantined. Quarantine publication uses a monotonically increasing sequence and root digest; gateways acknowledge the exact sequence and root before convergence is declared.
