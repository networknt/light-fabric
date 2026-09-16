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

## Accepted document publication

`POST /v1/publications` accepts a typed `PublicationDelivery`; `POST
/v1/publications/status` reconciles the identical delivery without issuing a
write. Both require the service token. Publication is disabled unless
`GITHUB_ACTION_PROVIDER_PUBLICATION_POLICY` explicitly allows the repository,
document branch/path and issue/comment capabilities. A durable intent precedes
each first write. Lost responses and restarts perform remote observation, not
another POST or PUT. Changed retries conflict; documents must be new immutable
paths and return a verified commit permalink.

The optional `docker-compose.publication.yml` override is provided in
`light-portal-install` and `portal-config-loc/all-in-lt`. Include it after the
base Compose file. It uses a loopback listener in Workflow's network namespace,
exposes no host port, and persists the SQLite journal/work directory in
`github-publication-state`. Recreate both services together if Workflow's
network namespace changes. Run one provider replica only.

Required override variables are `LIGHT_FABRIC_WORKSPACE`,
`WORKFLOW_PUBLICATION_SERVICE_TOKEN_FILE`,
`WORKFLOW_PUBLICATION_GITHUB_TOKEN_FILE`, `WORKFLOW_PUBLICATION_REPOSITORIES`
and `WORKFLOW_PUBLICATION_POLICY`. Token sources must already exist as regular
files with mode `0600`, readable by the images' `workflow` user; bind mounts are
read-only and do not create missing host paths. Use a newly built Workflow image
with dispatcher support. The provider has its own local build tag; it does not
change the release tag in `docker-images.env`. Do not delete its volume when
recreating containers or diagnosing an uncertain publication.

After building the provider image, run
`node tests/publication-provider-container-gate.mjs` from `light-portal-install`
for isolated container recreation/lost-response checks. It uses a loopback mock
GitHub server, three disposable publication effects and unique test volumes;
it does not touch the Portal databases or create remote GitHub resources. This
provider-only gate does not replace owner-authenticated Workflow finalization or
full installer artifact/export qualification.

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
