# light-coding-agent-fixture

This is the first structured coding-adapter conformance fixture. It is
deterministic rather than model-driven: the purpose is to prove the complete
repository-input, Cube execution, and trusted canonical-patch boundary before
a model broker and Pi adapter are admitted.

The approved Cube template must contain this release binary at
`/usr/local/bin/light-coding-agent-fixture`, `/usr/bin/git`, and envd 0.5.7 or
newer. Build the binary in release mode, copy that exact binary into the image,
record the image/template digest in the runner compatibility record, and add
the canonical command template digest to the runner allowlist.

`Dockerfile.cube` is the minimal overlay for an approved Cube envd base image:

```bash
cargo build --release -p light-coding-agent-fixture
docker build -f apps/light-coding-agent-fixture/Dockerfile.cube \
  --build-arg BASE_IMAGE=<pinned-envd-image@sha256:digest> \
  -t <registry>/light-coding-fixture:<immutable-version> .
```

Publish by digest, then build a Cube template from that digest. Mutable image
tags are not valid compatibility evidence.

Run the live gate against a non-production Cube cluster:

```bash
LIGHT_CUBE_TEST_API_URL=https://cube-api.internal.example/ \
LIGHT_CUBE_TEST_SANDBOX_URL=https://cube-sandbox.internal.example/ \
LIGHT_CUBE_TEST_API_KEY_FILE=/run/secrets/cube-api.key \
LIGHT_CUBE_TEST_TLS_CA_FILE=/run/secrets/cube-ca.pem \
LIGHT_CUBE_TEST_TEMPLATE_ID=<immutable-template-id> \
cargo test -p execution-backend-cube \
  --test cube_coding_live \
  immutable_repository_returns_canonical_patch_through_live_cube \
  -- --ignored --nocapture
```

The test creates a fresh repository and Git bundle, hashes and uploads the
bundle to `/inputs/repository.bundle`, executes the fixture with deny-all
network access, verifies the base commit inside the guest, edits one regular
file, exports a binary-safe Git patch, independently validates its paths and
digest on the runner, and synchronously deletes the sandbox.

For repeatable execution, use `bash ci/run-execution-live-matrix.sh cube`. The
entrypoint validates every required setting, runs both the Cube lifecycle
failure matrix and this coding fixture serially, and applies a hard timeout to
each test binary.

The dedicated `.github/workflows/execution-live-matrix.yml` pipeline compiles
the ignored tests on relevant pull requests and executes them weekly or through
`workflow_dispatch` on a runner labelled `light-execution-integration`.
Configure API and template locations as protected `execution-integration`
environment variables and configure `LIGHT_CUBE_TEST_API_KEY` and
`LIGHT_CUBE_TEST_TLS_CA_PEM` as environment secrets. The workflow materializes
those values into owner-only temporary files, removes them after the test, and
retains the test log for 30 days. The Cube template native TTL remains the
cleanup backstop if a CI worker is killed before synchronous cleanup completes.
