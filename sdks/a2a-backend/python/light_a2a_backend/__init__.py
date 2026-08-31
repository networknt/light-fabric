"""Production Python server adapter for light-a2a-backend/v1."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import sqlite3
import threading
import time
import uuid
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterable, Protocol

CONTRACT_VERSION = "light-a2a-backend/v1"
CONTEXT_HEADER = "x-light-a2a-backend-context"
SIGNATURE_HEADER = "x-light-a2a-backend-signature"
CONTRACT_DIGEST_HEADER = "x-light-a2a-backend-contract-digest"


class AgentBackend(Protocol):
    def capabilities(self) -> dict[str, Any]: ...
    def invoke(self, context: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]: ...
    def invoke_stream(self, context: dict[str, Any], request: dict[str, Any]) -> Iterable[dict[str, Any]]: ...
    def status(self, context: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]: ...
    def cancel(self, context: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]: ...


class ReplayStore:
    """Restart-safe replay store; one SQLite file must be private to a backend."""

    def __init__(self, path: Path):
        self._lock = threading.Lock()
        self._db = sqlite3.connect(path, check_same_thread=False)
        self._db.execute("CREATE TABLE IF NOT EXISTS invocation_replay(id TEXT PRIMARY KEY, expires REAL NOT NULL)")
        self._db.commit()

    def consume(self, invocation_id: str, expires: float) -> bool:
        with self._lock:
            now = time.time()
            self._db.execute("DELETE FROM invocation_replay WHERE expires <= ?", (now,))
            try:
                self._db.execute("INSERT INTO invocation_replay VALUES(?,?)", (invocation_id, expires))
                self._db.commit()
                return True
            except sqlite3.IntegrityError:
                self._db.rollback()
                return False


@dataclass(frozen=True)
class AdapterConfig:
    key: bytes
    contract_digest: str
    audience: str
    host_id: str
    environment: str
    target_agent_ref: str
    binding_id: str
    publication_id: str
    policy_digest: str
    data_boundary_digest: str
    replay_store: ReplayStore
    maximum_request_bytes: int = 1_048_576


def _decode_time(value: str) -> float:
    from datetime import datetime
    return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def _digest(value: Any) -> bool:
    return isinstance(value, str) and len(value) == 71 and value.startswith("sha256:") and all(c in "0123456789abcdef" for c in value[7:])


def verify(headers: Any, body: bytes, operation: str, config: AdapterConfig) -> dict[str, Any]:
    encoded = headers.get(CONTEXT_HEADER)
    signature = headers.get(SIGNATURE_HEADER)
    if not encoded or not signature or headers.get(CONTRACT_DIGEST_HEADER) != config.contract_digest:
        raise PermissionError("missing or mismatched signed invocation headers")
    expected = hmac.new(config.key, encoded.encode() + b"\0" + body, hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expected, signature):
        raise PermissionError("signature mismatch")
    padded = encoded + "=" * (-len(encoded) % 4)
    context = json.loads(base64.urlsafe_b64decode(padded))
    request = json.loads(body)
    context_fields = {"contractVersion","invocationId","issuer","audience","hostId","environment","principalSubject","callerAgentRef","targetAgentRef","bindingId","publicationId","selectedSkillId","operation","taskId","contextId","idempotencyKey","backendOperationId","policyDigest","dataBoundaryDigest","requestDigest","budget","traceparent","issuedAt","deadline","expiresAt"}
    request_fields = {"taskId","contextId","idempotencyKey","skillId","message","metadata"}
    if not isinstance(context, dict) or set(context) != context_fields or not isinstance(request, dict) or set(request) != request_fields:
        raise PermissionError("unknown or missing contract field")
    try:
        for key in ("invocationId","hostId","bindingId","publicationId","taskId","contextId"):
            uuid.UUID(context[key])
    except (ValueError, TypeError, KeyError):
        raise PermissionError("invalid UUID identity") from None
    now = time.time()
    fixed = {
        "contractVersion": CONTRACT_VERSION, "issuer": "light-a2a",
        "audience": config.audience, "hostId": config.host_id,
        "environment": config.environment, "targetAgentRef": config.target_agent_ref,
        "bindingId": config.binding_id, "operation": operation,
        "publicationId": config.publication_id, "policyDigest": config.policy_digest,
        "dataBoundaryDigest": config.data_boundary_digest,
    }
    if any(context.get(key) != value for key, value in fixed.items()):
        raise PermissionError("invocation binding mismatch")
    issued = _decode_time(context["issuedAt"])
    deadline = _decode_time(context["deadline"])
    expires = _decode_time(context["expiresAt"])
    budget = context.get("budget")
    if (issued > now + 30 or deadline <= now or expires <= now or expires > deadline
            or expires > issued + 300
            or any(not isinstance(context.get(key), str) or not context[key].strip()
                   for key in ("principalSubject", "callerAgentRef", "targetAgentRef", "environment", "idempotencyKey"))
            or any(not _digest(context.get(key)) for key in ("policyDigest", "dataBoundaryDigest", "requestDigest"))
            or not isinstance(budget, dict)
            or any(not isinstance(budget.get(key), int) or budget[key] < 1
                   for key in ("maximumInputBytes", "maximumOutputBytes", "maximumArtifactBytes"))):
        raise PermissionError("invalid invocation envelope")
    if context.get("requestDigest") != "sha256:" + hashlib.sha256(body).hexdigest():
        raise PermissionError("request digest mismatch")
    for key in ("taskId", "contextId", "idempotencyKey"):
        if request.get(key) != context.get(key):
            raise PermissionError(f"unsigned {key}")
    if request.get("skillId") != context.get("selectedSkillId"):
        raise PermissionError("unsigned skillId")
    if operation in ("STATUS", "CANCEL") and not context.get("backendOperationId"):
        raise PermissionError("missing backend operation identity")
    if not config.replay_store.consume(context["invocationId"], expires):
        raise PermissionError("replayed invocation")
    return context


def serve(backend: AgentBackend, config: AdapterConfig, port: int) -> ThreadingHTTPServer:
    """Start a loopback-only HTTP server and return it to the caller."""
    if len(config.key) < 32 or port < 1:
        raise ValueError("invalid adapter key or port")

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_: Any) -> None:
            return

        def _json(self, status: int, value: Any) -> None:
            data = json.dumps(value, separators=(",", ":")).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self) -> None:
            if self.path == "/v1/capabilities":
                if self.headers.get(CONTRACT_DIGEST_HEADER) != config.contract_digest:
                    self._json(401, {"code": "BACKEND_INVOCATION_REJECTED", "message": "contract digest mismatch", "retryable": False})
                else:
                    self._json(200, backend.capabilities())
            elif self.path in ("/health/live", "/health/ready"):
                self.send_response(204); self.end_headers()
            else:
                self._json(404, {"code": "NOT_FOUND", "message": "fixed route not found", "retryable": False})

        def do_POST(self) -> None:
            operations = {"/v1/invoke": "INVOKE", "/v1/invoke-stream": "INVOKE_STREAM", "/v1/status": "STATUS", "/v1/cancel": "CANCEL"}
            operation = operations.get(self.path)
            length = int(self.headers.get("content-length", "-1"))
            if operation is None or length < 0 or length > config.maximum_request_bytes:
                self._json(400, {"code": "INVALID_REQUEST", "message": "invalid route or body size", "retryable": False}); return
            body = self.rfile.read(length)
            try:
                context = verify(self.headers, body, operation, config)
                request = json.loads(body)
                if operation == "INVOKE_STREAM":
                    self.send_response(200); self.send_header("content-type", "text/event-stream"); self.send_header("cache-control", "no-store"); self.end_headers()
                    previous = 0
                    terminal = False
                    for event in backend.invoke_stream(context, request):
                        sequence = int(event["sequenceNumber"])
                        if sequence <= previous: raise ValueError("events are not ordered")
                        previous = sequence
                        terminal = event.get("terminal") is True
                        self.wfile.write(b"data: " + json.dumps(event, separators=(",", ":")).encode() + b"\n\n"); self.wfile.flush()
                    if not terminal:
                        raise ValueError("stream did not terminate")
                    return
                callback = {"INVOKE": backend.invoke, "STATUS": backend.status, "CANCEL": backend.cancel}[operation]
                self._json(200, callback(context, request))
            except PermissionError as error:
                self._json(401, {"code": "BACKEND_INVOCATION_REJECTED", "message": str(error), "retryable": False})
            except Exception as error:
                self._json(422, {"code": "BUSINESS_ERROR", "message": str(error), "retryable": False})

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server
