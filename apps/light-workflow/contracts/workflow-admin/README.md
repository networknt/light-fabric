# Workflow admin contracts

These Phase 0 contracts freeze the public Gateway and private Workflow tool shapes before handler or transport work. Gateway and Workflow expose the same names and schemas. Identity and Host come only from the trusted invocation context, never from tool arguments.

Run `npm ci && npm test` in this directory. AJV validates the complete draft 2020-12 schemas and examples. Custom checks then enforce the manifest, stable error registry, identifier separation, bounded pagination, mutation concurrency, and the absence of identity/fencing arguments after resolving schema references.

Contract version `0.2.0-restoration` records the reviewed restoration semantics. A successful human-task mutation reports that its completion was durably recorded; asynchronous executor continuation remains observable through process/task reads. Changing names, required fields, identifier meanings, error codes, or bounds requires a reviewed contract version and updated consumers/fixtures.
