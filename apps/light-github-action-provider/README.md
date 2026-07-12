# Light GitHub Action Provider

Concrete credential-owning provider for Light Workflow `create-branch` and
`open-pr` fixed actions. It accepts only allowlisted repositories and the
configured branch prefix, journals the immutable request before calling
GitHub, forwards no credential to the workflow service, and reconciles lost
responses through GitHub using the durable `Idempotency-Key`.

The provider does not create an empty branch. It verifies the approved patch
digest, clones the allowlisted repository into a fresh hook/filter/submodule-
disabled workspace, applies the patch against the exact approved base commit,
creates a deterministic commit, and compare-and-set pushes a new branch. An
`open-pr` operation inspects that branch before creating the pull request and
accepts success only when the PR head is the exact deterministic commit. A
retry reconstructs the same commit and reconciles GitHub state; it never
force-updates an existing branch.

Required configuration:

- `GITHUB_ACTION_PROVIDER_DB`
- `GITHUB_ACTION_PROVIDER_SERVICE_TOKEN_FILE`
- `GITHUB_ACTION_PROVIDER_TOKEN_FILE`
- `GITHUB_ACTION_PROVIDER_WORK_ROOT`, an owner-only directory used for fresh
  trusted Git workspaces
- `GITHUB_ACTION_PROVIDER_REPOSITORIES`, a JSON object mapping approved clone
  URLs to `{ "owner": "...", "repo": "..." }`

Optional: `GITHUB_ACTION_PROVIDER_ADDR` (default `0.0.0.0:8450`),
`GITHUB_ACTION_PROVIDER_API_URL`, and `GITHUB_ACTION_PROVIDER_BRANCH_PREFIX`
(default `agent/`). Secret files must be owner-only regular files. Configure
Light Workflow with a base URL ending in `/v1/`.

The host must provide `git`. Canonical patch input is bounded at 16 MiB, and
the HTTP service accepts only enough request body space for that bounded patch
plus its typed metadata.

The current journal is intentionally local and synchronous. Deploy this first
provider as one active replica with a durable local volume; do not place
multiple active replicas behind a load balancer because status requests must
reach the journal that admitted the operation. GitHub-side reconciliation
still protects a restarted replica from repeating branch/PR effects. A future
HA deployment should replace the journal with a shared transactional store
without changing the HTTP or idempotency contract.
