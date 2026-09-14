# A2 local workflow action profile

This opt-in profile activates the frozen issue #374 dual-identity contracts for
the personal stack. It uses separate workflow-only Codex and Claude service and
Agent identities. Private keys and bearer tokens stay in `.runtime/active`.

Run `python3 prepare.py --output .runtime/active` while the local PostgreSQL
container is available. Review `manifest.json`, then create `.runtime/enabled`.
The normal `deploy-local.sh lt` command includes `compose.yml` when that marker
exists. Apply operational migration `0007_workflow_action_dispatch` before the
new Workflow image starts.

The preparation step creates a local A2 CA and ten-year development certificates
and app tokens signed by the local issuer, matching the local stack's checkout-and-
run fixture policy. Official environments must use managed PKI and issuer-created
app credentials with their approved rotation policy instead.
