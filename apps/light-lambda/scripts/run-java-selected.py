#!/usr/bin/env python3
"""Run explicit fixture IDs with the unchanged native Java conformance runner."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--out", required=True, type=Path)
parser.add_argument("--fixture", required=True, action="append")
args = parser.parse_args()
repo = Path(__file__).resolve().parents[3]
native = repo.parent / "light-lambda-native-conformance"
by_id = {json.loads(p.read_text())["id"]: p for p in (repo / "tests/serverless-conformance/fixtures").rglob("*.json")}
if len(set(args.fixture)) != len(args.fixture):
    parser.error("duplicate fixture selection")
for name in args.fixture:
    if name not in by_id:
        parser.error("unknown fixture: " + name)
    if name == "L-02-synthetic-address-counter-configured-v1":
        parser.error("preserved Java L-02 timing failure is excluded from this delivery helper")
out = args.out.resolve()
out.mkdir()
results = out / "results"
results.mkdir()
commands = []

def execute(name, argv, budget):
    start = time.monotonic()
    with (out / (name + ".log")).open("x") as log:
        child = subprocess.Popen(["rtk", "proxy", *map(str, argv)], cwd=native, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = child.wait(timeout=budget)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            code = 124
    record = dict(name=name, argv=list(map(str, argv)), exit=code, elapsedSeconds=time.monotonic()-start)
    with (out / (name + ".json")).open("x") as f:
        json.dump(record, f, indent=2)
    commands.append(record)
    return code

if execute("test-compile", ["mvn", "-o", "-q", "test-compile", "dependency:build-classpath", "-Dmdep.outputFile=target/conformance-cp.txt"], 180):
    raise SystemExit("native test compilation failed; see " + str(out))
classpath = os.pathsep.join([str(native / "target/test-classes"), str(native / "target/classes"), (native / "target/conformance-cp.txt").read_text().strip()])
revision = subprocess.check_output(["rtk", "git", "rev-parse", "HEAD"], cwd=native, text=True).strip()
failed = 0
for name in args.fixture:
    fixture = by_id[name]
    contract = json.loads(fixture.read_text())
    prerequisite = contract.get("insecureTestConfigPrerequisite")
    if prerequisite not in (None, "allow-v1"):
        raise SystemExit("unsupported test-config prerequisite: " + name)
    environment = ["env", "-u", "LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG", "-u", "AWS_LAMBDA_FUNCTION_NAME", "AWS_EC2_METADATA_DISABLED=true", "AWS_REGION=us-east-1", "AWS_DEFAULT_REGION=us-east-1"]
    if prerequisite == "allow-v1":
        environment.append("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG=1")
    if "runtimeIdentity" in contract:
        identity = contract["runtimeIdentity"]
        if set(identity) != {"functionName"} or not isinstance(identity["functionName"], str) or not identity["functionName"]:
            raise SystemExit("unsupported runtime identity: " + name)
        environment.append("AWS_LAMBDA_FUNCTION_NAME=" + identity["functionName"])
    with tempfile.TemporaryDirectory(prefix="native-selected-conformance-") as config:
        argv = [*environment, "java",
                "-Dconformance.home=" + str(repo / "tests/serverless-conformance"),
                "-Dlogback.configurationFile=" + str(native / "src/test/resources/conformance-logback.xml"),
                "-Dconformance.runnerRevision=" + revision, "-Dlight-4j-config-dir=" + config,
                "-cp", classpath, "com.networknt.aws.lambda.conformance.ConformanceRunnerMain", str(fixture), str(results / fixture.name)]
        failed += execute(name, argv, 45) != 0
with (out / "commands.json").open("x") as f:
    json.dump(commands, f, indent=2)
print(f"Native selected fixtures: {len(args.fixture)-failed} passed, {failed} failed; results={results}")
raise SystemExit(bool(failed))
