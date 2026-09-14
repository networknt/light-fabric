#!/usr/bin/env python3
"""Sync the non-secret A2 profile into both personal distributions."""
from pathlib import Path
import shutil

source=Path(__file__).resolve().parent
workspace=source.parents[2]
for root in (workspace/'portal-config-loc/all-in-lt',workspace/'light-portal-install'):
    target=root/'workflow-actions'; target.mkdir(exist_ok=True)
    for name in ('README.md','identities.json','prepare.py','compose.yml'):
        shutil.copyfile(source/name,target/name)
    (target/'.gitignore').write_text('.runtime/\n__pycache__/\n')
print('A2 workflow action profile synchronized to both distributions.')
