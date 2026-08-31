#!/usr/bin/env python3
"""Fail unless every production SDK reports the same complete TCK case set."""

import json
import sys
from pathlib import Path

if len(sys.argv) != 2:
    raise SystemExit("usage: verify_reports.py REPORT_DIRECTORY")

root = Path(__file__).resolve().parent
required = set(json.loads((root / "cases.json").read_text())["requiredCases"])
report_dir = Path(sys.argv[1])
expected_build = None
for sdk in ("python", "typescript", "java"):
    path = report_dir / f"{sdk}.json"
    if not path.is_file():
        raise SystemExit(f"missing {sdk} TCK report")
    report = json.loads(path.read_text())
    if report.get("contract") != "light-a2a-backend/v1":
        raise SystemExit(f"{sdk} reported the wrong contract")
    build = report.get("lightA2aBuildSha256")
    if not isinstance(build, str) or len(build) != 64:
        raise SystemExit(f"{sdk} did not report a light-a2a build digest")
    expected_build = expected_build or build
    if build != expected_build:
        raise SystemExit(f"{sdk} ran against a different light-a2a build")
    covered = set(report.get("coveredCases", []))
    if covered != required:
        raise SystemExit(f"{sdk} TCK coverage mismatch: missing={sorted(required-covered)} extra={sorted(covered-required)}")
print("A2A backend SDK TCK coverage PASS")
