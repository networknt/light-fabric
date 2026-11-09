#!/usr/bin/env python3
"""Check a locally built gateway image's default CA; requires PyYAML."""
import argparse
from pathlib import Path
import re
import subprocess

import yaml

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("image", help="locally built gateway image")
args = parser.parse_args()
config = Path(__file__).resolve().parents[1] / "apps/light-gateway/config"
policy = yaml.safe_load((config / "spa-token-trust.yml").read_text())
setting = policy["internal"]["jwksCaPaths"]
match = re.fullmatch(r"\$\{spa-token-trust\.internal\.jwksCaPaths:(/[^{}]+)\}", setting)
if not match:
    parser.error("expected a single absolute internal CA default in the shipped template")
ca_path = match[1]
# Retain the image USER; no gateway startup, networking or host mounts.
result = subprocess.run(
    ["docker", "run", "--rm", "--pull=never", "--network", "none", "--read-only",
     "--entrypoint", "/bin/cat", args.image, ca_path],
    check=True, stdout=subprocess.PIPE,
)
if result.stdout != (config / "ca.pem").read_bytes():
    parser.exit(1, f"CA bytes at {ca_path} in {args.image} differ from the shipped development CA\n")
print(f"Gateway image CA matches the shipped development CA at {ca_path}")
