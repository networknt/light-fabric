# Proposed Workflow MCP runtime contracts, version 1

This is the Step 01 frozen implementation contract for the September 24 execution guide. It is deliberately separate from `../tool-manifest.json`: that manifest advertises implemented tools and must not list these proposed operations until their handlers, Gateway routing and publication pass later steps.

Run `npm test` in this directory. The validator compiles every draft 2020-12 schema, checks positive and negative examples, confirms the proposed tool roster, checks forbidden caller authority fields, and compares the retirement keys with the source inventory. It uses the AJV installation in the parent `workflow-admin` contract package; install that package with `npm ci` first if needed.

`contracts.json` freezes names, permissions, error codes and gate owners. `schemas.json` freezes wire shapes. `examples.json` is synthetic contract data, not a runtime success claim. The decisions and unresolved live evidence are recorded in `implementation/light-workflow/workflow-mcp-execution/steps/01.md`.
