# GitHub API Workflow integration

Integrates Host Tool publication, atomic accepted authority and recovery, Gateway MCP transport, and the exact operational 2.7.0 / 45-migration bundle. PostgreSQL tests are explicitly opt-in; the maintained CI runner requires four operational plus three publication/token/recovery tests to execute with zero failures or ignored tests. The synthetic Portal handoff is maintained beside the tests. Runtime/security behavior is the qualified delivery.

Merge order: lightapi/portal-db, networknt/light-fabric, lightapi/light-portal,
lightapi/portal-view, lightapi/portal-config-loc, lightapi/light-portal-install.
Portal schema-dependent CI must use the merged Portal DB schema. Deployment
copies depend on the merged Fabric release; UI depends on Portal contracts.

Catalog upgrade order is endpoint-resolution, then Host Tool access, through
supported migration tooling with verified backups. Operational order 45 is
0026_host_tool_workflow_access.sql, SHA-256
d7a0c9a82e698d5d2d09849fcc5ed40fe530aa4296d4d66cc0f97f6dacc6ae9b.
The bundle manifest/order/checksum identities and historical 2.6.0 contracts
remain frozen. Current catalog inventories are synchronized across package
copies; descriptor/tool pins are updated only for that combined inventory.

Upgrade all start-accepting/executing workers and v7 event/replay consumers
before enabling broad access. Preserve caller authentication, Gateway ACLs,
specific grants, exact pins, admission settings and existing accepted runs.
There is no background publisher. Catalog pending/error status is fail-closed;
an operational disable takes effect only after publication commits.

Rollback is forward: publish disabled policy tombstones, retain additive
schema and v7-aware consumers, and retain upgraded workers for accepted runs.
A full binary/schema downgrade after broad acceptance or v7 events is not
supported. Never repair projections or accepted authority through direct SQL.

Qualification originated in isolated fixtures and a retained successful local
API-only Workflow run (two GETs, one comments page). Integration validation and
hosted CI are separate evidence. Native-agent/model work, full pagination,
installer acceptance and wider runtime/security qualification are not claimed.

Owner rebuild, deployment, make all and fresh live Workflow testing remain
pending. Integration does not apply application migrations, restart services,
change admission/grants/policies, or select freshly built images.

Mandatory disposable database lane, from this repository:

```sh
python3 scripts/qualify-host-tool-workflow-access.py --output-dir /absolute/path/to/new-evidence
```

It creates only a labelled synthetic PostgreSQL fixture, requires all seven
selected tests, and removes its owned container/volume. The standard Cargo
test lane skips the four fixture integration tests; those skips are not proof.
