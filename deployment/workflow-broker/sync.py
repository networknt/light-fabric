#!/usr/bin/env python3
"""Sync the A1 assets and canonical fresh-install SQL to both local distributions."""
from pathlib import Path
import shutil

source = Path(__file__).resolve().parent
fabric = source.parents[1]
workspace = fabric.parent
shutil.copyfile(workspace / "portal-service/apps/light-oauth/config/server.yml", source / "issuer-server.yml")
shutil.copyfile(fabric / "apps/light-workflow/migrations/credential_broker.sql", source / "credential_broker.sql")
ddl = (workspace / "portal-db/postgres/ddl.sql").read_text()
seed = (workspace / "portal-db/postgres/init-lightapi.sql").read_text()
import re
if re.search(r"(?m)^\s*\\i(?:r)?\s", ddl + seed):
    raise SystemExit("use the canonical include-expanding bootstrap generator")
bootstrap = "CREATE DATABASE configserver;\n\\c configserver;\n\n" + ddl + "\n\n" + seed
for root in (workspace / "portal-config-loc/all-in-lt", workspace / "light-portal-install"):
    target = root / "workflow-broker"
    target.mkdir(exist_ok=True)
    for name in ("prepare.py", "set-ownership.py", "refresh-claims-preflight.sql", "issuer-server.yml", "credential_broker.sql", "local-profile.json", "compose.yml", "README.md"):
        shutil.copyfile(source / name, target / name)
    (root / "postgres-db/init.sql").write_text(bootstrap)
print("A1 assets and canonical bootstrap SQL synchronized to both distributions.")
