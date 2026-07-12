# Light GitHub Action Provider

Concrete credential-owning provider for Light Workflow `create-branch` and
`open-pr` fixed actions. It accepts only allowlisted repositories and the
configured branch prefix, journals the immutable request before calling
GitHub, forwards no credential to the workflow service, and reconciles lost
responses through GitHub using the durable `Idempotency-Key`.

Required configuration:

- `GITHUB_ACTION_PROVIDER_DB`
- `GITHUB_ACTION_PROVIDER_SERVICE_TOKEN_FILE`
- `GITHUB_ACTION_PROVIDER_TOKEN_FILE`
- `GITHUB_ACTION_PROVIDER_REPOSITORIES`, a JSON object mapping approved clone
  URLs to `{ "owner": "...", "repo": "..." }`

Optional: `GITHUB_ACTION_PROVIDER_ADDR` (default `0.0.0.0:8450`),
`GITHUB_ACTION_PROVIDER_API_URL`, and `GITHUB_ACTION_PROVIDER_BRANCH_PREFIX`
(default `agent/`). Secret files must be owner-only regular files. Configure
Light Workflow with a base URL ending in `/v1/`.

The current journal is intentionally local and synchronous. Deploy this first
provider as one active replica with a durable local volume; do not place
multiple active replicas behind a load balancer because status requests must
reach the journal that admitted the operation. GitHub-side reconciliation
still protects a restarted replica from repeating branch/PR effects. A future
HA deployment should replace the journal with a shared transactional store
without changing the HTTP or idempotency contract.
