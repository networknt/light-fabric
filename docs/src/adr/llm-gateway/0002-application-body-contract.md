# ADR 0002: One-Pass Application Body Contract

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before buffered HTTP integration

## Decision

Register one `llm` application handler and delegate to a typed integration.
The integration runs pre-body handlers, validates route/method/media type,
content encoding, declared length, and deadline, then captures/decompresses one
bounded `Bytes` body exactly once.

If access control appeared earlier in the selected chain, body-aware
authorization receives that captured byte sequence before LLM JSON parsing,
alias policy, transforms, client selection, or provider work. Parsing and all
later content adapters borrow or clone the same immutable `Bytes`; they do not
read the downstream stream again. Every error and downstream disconnect
cancels/finalizes the request.

Generic tokenize/detokenize handlers are not assumed to have consumed the body.
Content transforms require an explicit LLM adapter.

## Evidence

`benchmarks/llm-gateway/evidence/body-capture.json` records bounded,
chunked, one-pass capture and proves authorization precedes parsing while both
observe the same digest. The production gateway already demonstrates the
relevant ordering in `GatewayProxy::request_body_filter`; LF-4 must bind this
ADR to the new application handler with an integration test.

