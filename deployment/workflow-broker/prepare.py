#!/usr/bin/env python3
"""Prepare private A1 files and reviewable SQL; never connects to a database.

Supply administrator-approved certificates. This tool deliberately does not
generate a CA, import Portal events, activate a snapshot, or restart services.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import uuid
from urllib.parse import urlparse, unquote


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def prepare(manifest, source, output):
    m = json.loads(manifest.read_text())
    for field in ("authHostId", "tenantId", "clientId", "ownerId"):
        uuid.UUID(m[field])
    if m["profile"] != "local-workflow-user-v1":
        raise ValueError("unsupported issuer profile")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,22}", m["providerId"]):
        raise ValueError("invalid provider identifier")
    if not 600 <= m["maximumGrantSeconds"] <= 31536000:
        raise ValueError("invalid grant duration ceiling")
    for key in ("authorizationBaseUrl", "brokerBaseUrl", "callbackUri"):
        url = urlparse(m[key])
        if url.scheme != "https" or not url.hostname or url.username or url.password or url.query or url.fragment:
            raise ValueError("invalid HTTPS endpoint: " + key)
    if not m["callbackUri"].endswith("/workflow/credentials/callback"):
        raise ValueError("invalid callback path")
    if not m["scope"] or not m["sanUri"].startswith("spiffe://"):
        raise ValueError("scope and registered workload URI are required")
    # A prepared directory is immutable. Never silently rotate an encryption key.
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    for component in ("issuer", "workflow"):
        (output / component).mkdir(mode=0o700)
    copies = {
        "issuer/server.pem": "server.pem", "issuer/server.key": "server.key",
        "issuer/client-ca.pem": "client-ca.pem",
        "workflow/client-identity.pem": "client-identity.pem",
        "workflow/issuer-ca.pem": "issuer-ca.pem",
        "workflow/database-url": "database-url",
        "workflow/callback.pem": "callback.pem", "workflow/callback.key": "callback.key",
    }
    for target, name in copies.items():
        data = (source / name).read_bytes()
        if not data:
            raise ValueError("empty input: " + name)
        (output / target).write_bytes(data)
        (output / target).chmod(0o600)
    cert = subprocess.run(["openssl", "x509", "-in", str(source / "client-identity.pem"), "-outform", "DER"], check=True, capture_output=True).stdout
    san = subprocess.run(["openssl", "x509", "-in", str(source / "client-identity.pem"), "-noout", "-ext", "subjectAltName"], check=True, capture_output=True).stdout.decode()
    if ("URI:" + m["sanUri"]) not in [v.strip() for v in san.splitlines()[-1].split(",")]:
        raise ValueError("client certificate SAN does not match registration")
    fingerprint = hashlib.sha256(cert).hexdigest()
    ring = {"activeKeyId": "initial", "keys": {"initial": base64.b64encode(secrets.token_bytes(32)).decode()}}
    (output / "workflow/keyring.json").write_text(json.dumps(ring))
    (output / "workflow/keyring.json").chmod(0o600)
    broker = m["brokerBaseUrl"].rstrip("/") + "/oauth2/" + m["providerId"]
    public = m["authorizationBaseUrl"].rstrip("/") + "/oauth2/" + m["providerId"]
    settings = {"callbackUri": m["callbackUri"],
        "legacyLongLivedAppKeys": m.get("legacyLongLivedAppKeys", []),
        "callbackTls": {"address":"0.0.0.0:8447", "certificateFile":"/run/workflow-broker/callback.pem", "privateKeyFile":"/run/workflow-broker/callback.key"},
        "databaseUrlFile": "/run/workflow-broker/database-url",
        "keyringFile": "/run/workflow-broker/keyring.json", "provider": {
        "authorizationUrl": public + "/workflow/authorize", "tokenUrl": broker + "/token",
        "enrollmentUrl": broker + "/workflow/enrollments", "grantUrlPrefix": broker + "/workflow/grants",
        "jwksUrl": public + "/keys", "clientId": m["clientId"], "issuer": m["issuer"], "audience": m["audience"],
        "clientIdentityFile": "/run/workflow-broker/client-identity.pem", "caFile": "/run/workflow-broker/issuer-ca.pem"}}
    (output / "workflow-settings.json").write_text(json.dumps(settings, indent=2) + "\n")
    # Mount over server.yml, preserving its ordinary-listener template fields.
    template = Path(__file__).parents[2] / "apps/light-oauth/config/server.yml"
    if not template.exists():
        template = Path(__file__).with_name("issuer-server.yml")
    lines = template.read_text().splitlines()
    lines = [line for line in lines if not line.startswith("workflowBroker:")]
    listener = {"address": "0.0.0.0:7443", "certificateFile": "/run/workflow-broker/server.pem",
        "privateKeyFile": "/run/workflow-broker/server.key", "clientCaFile": "/run/workflow-broker/client-ca.pem"}
    (output / "issuer/server.yml").write_text("workflowBroker: " + json.dumps(listener) + "\n" + "\n".join(lines) + "\n")
    # Existing registrations must match exactly; retries cannot revive a revoked client.
    h,c,p,o,t = (literal(m[k]) for k in ("authHostId","clientId","providerId","ownerId","tenantId"))
    scope,callback = literal(m["scope"]),literal(m["callbackUri"])
    check = "$a1_" + secrets.token_hex(16) + "$"
    while check in json.dumps(m):
        check = "$a1_" + secrets.token_hex(16) + "$"
    sql = f"""-- Apply after patch_20260913_01_workflow_broker.sql, with the issuer schema search_path.
BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended('workflow-broker-registration:' || {c},0));
INSERT INTO auth_client_t(host_id,client_id,client_name,owner_id,client_type,client_profile,client_secret,client_scope,redirect_uri)
VALUES({h},{c},'Workflow unattended broker',{o},'confidential','service','mTLS-only-no-secret-login',{scope},{callback})
ON CONFLICT(host_id,client_id) DO NOTHING;
DO {check} BEGIN
 IF NOT EXISTS(SELECT 1 FROM auth_client_t WHERE host_id={h} AND client_id={c} AND active
   AND owner_id={o} AND client_type='confidential' AND client_profile='service'
   AND client_scope={scope} AND redirect_uri={callback} AND custom_claim IS NULL) THEN
   RAISE EXCEPTION 'broker client registration conflicts'; END IF;
END {check};
INSERT INTO auth_provider_client_t(host_id,client_id,provider_id) VALUES({h},{c},{p}) ON CONFLICT DO NOTHING;
INSERT INTO auth_workflow_broker_t(auth_host_id,provider_id,client_id,allowed_host_ids,maximum_grant_seconds)
VALUES({h},{p},{c},ARRAY[{t}::uuid],{m['maximumGrantSeconds']}) ON CONFLICT DO NOTHING;
DO {check} BEGIN
 IF NOT EXISTS(SELECT 1 FROM auth_provider_client_t WHERE host_id={h} AND client_id={c} AND provider_id={p} AND active)
 OR NOT EXISTS(SELECT 1 FROM auth_workflow_broker_t WHERE auth_host_id={h} AND client_id={c} AND provider_id={p}
  AND active AND allowed_host_ids=ARRAY[{t}::uuid] AND maximum_grant_seconds={m['maximumGrantSeconds']}) THEN
  RAISE EXCEPTION 'broker provider or ceiling conflicts'; END IF;
END {check};
INSERT INTO auth_workflow_broker_certificate_t(auth_host_id,provider_id,client_id,certificate_sha256,san_uri)
VALUES({h},{p},{c},{literal(fingerprint)},{literal(m['sanUri'])}) ON CONFLICT DO NOTHING;
DO {check} BEGIN
 IF NOT EXISTS(SELECT 1 FROM auth_workflow_broker_certificate_t WHERE auth_host_id={h} AND provider_id={p}
 AND client_id={c} AND certificate_sha256={literal(fingerprint)} AND san_uri={literal(m['sanUri'])} AND active) THEN
 RAISE EXCEPTION 'broker certificate is retired or conflicting'; END IF;
END {check};
COMMIT;
"""
    (output / "register.sql").write_text(sql)
    database = urlparse((source / "database-url").read_text().strip())
    if database.scheme not in ("postgres", "postgresql") or database.username != "workflow_broker_runtime" or database.path != "/workflow_credentials" or not database.password:
        raise ValueError("use the dedicated workflow_credentials database and workflow_broker_runtime role")
    password = literal(unquote(database.password))
    roles = "$a1_" + secrets.token_hex(16) + "$"
    while roles in password:
        roles = "$a1_" + secrets.token_hex(16) + "$"
    migration = Path(__file__).with_name("credential_broker.sql").read_text()
    # This file contains the runtime password and is written privately. Its
    # administrator connection must point at the same PostgreSQL server.
    storage = f"""\\set ON_ERROR_STOP on
DO {roles} BEGIN
 IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='workflow_broker_owner') THEN
  CREATE ROLE workflow_broker_owner NOLOGIN; END IF;
 IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='workflow_broker_runtime') THEN
  CREATE ROLE workflow_broker_runtime LOGIN PASSWORD {password}; END IF;
 IF EXISTS(SELECT FROM pg_roles WHERE rolname IN ('workflow_broker_owner','workflow_broker_runtime')
  AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls))
 OR EXISTS(SELECT FROM pg_auth_members WHERE member IN
   (SELECT oid FROM pg_roles WHERE rolname='workflow_broker_runtime')) THEN
  RAISE EXCEPTION 'credential runtime has elevated privileges'; END IF;
END {roles};
SELECT 'CREATE DATABASE workflow_credentials OWNER workflow_broker_owner'
WHERE NOT EXISTS(SELECT FROM pg_database WHERE datname='workflow_credentials') \\gexec
\\connect workflow_credentials
DO $owner$ BEGIN
 IF NOT EXISTS(SELECT FROM pg_database d JOIN pg_roles r ON r.oid=d.datdba
 WHERE d.datname=current_database() AND r.rolname='workflow_broker_owner') THEN
 RAISE EXCEPTION 'credential database has an unexpected owner'; END IF;
END $owner$;
REVOKE ALL ON DATABASE workflow_credentials FROM PUBLIC;
GRANT CONNECT ON DATABASE workflow_credentials TO workflow_broker_runtime;
SET ROLE workflow_broker_owner;
{migration}
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA workflow_secret TO workflow_broker_runtime;
GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA workflow_secret TO workflow_broker_runtime;
RESET ROLE;
"""
    (output / "storage-bootstrap.sql").write_text(storage)
    (output / "storage-bootstrap.sql").chmod(0o600)
    evidence = dict(m, certificateSha256=fingerprint, tokenUse={"user":"user","app":"app"},
        allowedGrants=["authorization_code","refresh_token"], tokenEndpointAuthMethod="tls_client_auth",
        tokenLifetimeSeconds=600, migration="patch_20260913_01_workflow_broker.sql",
        status="prepared; activation and qualification receipts required")
    (output / "manifest.json").write_text(json.dumps(evidence, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    prepare(args.manifest, args.input, args.output)
