# light-a2a-backend/v1

This directory is the language-neutral authority for the private loopback
contract between `light-a2a` and an external business agent. `openapi.yaml`,
the referenced JSON Schemas, and the checked-in fixtures are versioned as one
unit. The SHA-256 digest of `openapi.yaml` is pinned in every activated backend
transport profile and sent on every business operation.

The backend never receives a caller credential or a Portal policy document.
Every operation instead carries a short-lived signed context which binds the
business request to one host, environment, publication, agent, optional skill,
operation, task, context, idempotency key, policy digest, data boundary,
deadline, and resource budget.

`openapi.sha256` is the publication value (prefixed with `sha256:` in Portal).
Run `sha256sum -c openapi.sha256` from this directory before publishing a
backend transport profile. Any contract edit requires a new digest and an
explicit profile generation; it is never inferred by a running backend.

`tck/cases.json` is the shared conformance case authority. Each production SDK
must execute every named case and emit a report; `tck/verify_reports.py`
compares all three reports with the manifest. The Phase 3 gate also builds one
`light-a2a` binary first and gives every SDK run the same binary digest, so a
mixed-build result cannot qualify a release.
