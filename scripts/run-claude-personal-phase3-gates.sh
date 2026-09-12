#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
workspace="$(dirname "$root")"
python3 "$workspace/portal-config-loc/tests/personal-runner-admission-test.py"
python3 "$workspace/portal-config-loc/tests/personal-runner-lifecycle-test.py"
python3 -m py_compile "$workspace/portal-config-loc/all-in-lt/light-workflow-runner-claude-personal/setup.py" "$workspace/light-portal-install/light-workflow-runner-claude-personal/setup.py"
cmp "$workspace/portal-config-loc/all-in-lt/light-workflow-runner-claude-personal/setup.py" "$workspace/light-portal-install/light-workflow-runner-claude-personal/setup.py"
cmp "$workspace/portal-config-loc/scripts/sync-personal-runner-admission.py" "$workspace/light-portal-install/scripts/sync-personal-runner-admission.py"
cmp "$workspace/portal-config-loc/scripts/personal-runner-lifecycle.py" "$workspace/light-portal-install/scripts/personal-runner-lifecycle.py"
bash -n "$workspace/portal-config-loc/scripts/deploy-local.sh" "$workspace/portal-config-loc/scripts/verify-agent-image.sh" "$workspace/light-portal-install/install.sh"
(cd "$workspace/light-portal" && mvn -q -pl db-provider -am test -Dtest=AgentCodingProfileTest,ClaudeCodingProfileTest -Dsurefire.failIfNoSpecifiedTests=false)
python3 - "$workspace" <<'PY'
import hashlib,pathlib,sys,yaml
root=pathlib.Path(sys.argv[1])
java=(root/'light-portal/db-provider/src/main/java/net/lightapi/portal/db/persistence/AgentCodingProfile.java').read_text()
for name in ['phase2-launch.json','phase2-qualification.json']:
    digest=hashlib.sha256((root/'light-fabric/contracts/claude-code/v2.1.269'/name).read_bytes()).hexdigest()
    assert 'sha256:'+digest in java, 'Portal and Rust Claude pins drifted: '+name
for distribution in ['portal-config-loc/all-in-lt','light-portal-install']:
    config=yaml.safe_load((root/distribution/'docker-compose.yml').read_text())
    service=config['services']['light-agent-claude-personal']
    assert service['environment']['LIGHT_AGENT_SERVICE_ID']=='com.networknt.agent.claude-personal-1.0.0'
    assert all('.claude:' not in str(v) for v in service['volumes'])
    assert any('service.jwt' in str(v) for v in service['volumes'])
    assert any('8090' in str(v) for v in service['ports'])
print('Both distributions have dedicated loopback Claude Agents and host-only native credentials.')
PY
mdbook build "$root/docs" --dest-dir /tmp/light-fabric-claude-phase3-book
printf '%s\n' 'Phase 3 configuration gates passed. Native deployment smoke is a separate opt-in gate.'
