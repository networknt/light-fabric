# Phase 1 Release Bundle

`bundle/` is generated from the canonical migrations in this crate and is the
exact directory staged into development deployment repositories. The
`manifest.json`, `migration-order.tsv`, and `bundle.sha256` files are all
validated before bootstrap. Deployment repositories must not edit copied SQL.

