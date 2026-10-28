# Phase-1 app template preparation

## Current authority — owner-approved WP10 amendment

The effective source is [apps/light-lambda/config/handler.yml](../../../apps/light-lambda/config/handler.yml).
This retained file is an exact synchronized reference copy. The earlier template
described below is preserved under the WP10 resume approval history; that
historical description is not the current route contract.

The owner approved preserving request/response/admin chains, removing obsolete
handler audit keys, omitting unavailable local routes, and reserving those paths
for all methods with local 404 and zero dispatch. The app uses the canonical
once-decoded core routing path, cannot bypass reservations with a proxy mapping,
and retains strict missing-terminal validation. These are unavailable endpoints,
not working health/admin handlers or Java parity. See the app README and WP10
resume evidence for built-binary execution and qualification limits.

## Earlier preparation history

`handler.yml` is the Java light-lambda-native handler template with only the
obsolete `auditOnError` / `auditStackTrace` block and its comments removed,
as authorized by the owner on 2026-10-12. Audit flags belong to `audit.yml`
through `audit.auditOnError` and `audit.auditStackTrace`.

Java cleanup issue: https://github.com/networknt/light-4j/issues/2811

The existing Rust light-gateway templates in the main checkout and Lambda
worktree already omit both keys. No existing Rust symbol was modified.

This template is staged here until WP10 creates the complete app package.
All Java chains and routes remain intact. Deferred health/admin terminal
disposition is still unresolved; this file is not evidence of a valid
Phase-1 execution plan or a completed app.

Verification:

- Exact comparison with Java: only the obsolete audit block was removed.
- Rust handler-template inventory: neither existing gateway template nor
  this staged template contains the obsolete keys.
- `rtk cargo test -p serverless-core --test plan_compiler --locked`: exit 0,
  12 passed. These tests enforce audit ownership and reject handler audit keys.
- The initial test command exited 101 because staging under `apps/light-lambda`
  without a manifest matched the workspace `apps/*` glob. The template was
  moved here and only this task's empty app directories were removed; the
  subsequent compiler test passed. No manifest or dummy binary was introduced.

No Java source edits, commits, deployment, or live-service qualification.
