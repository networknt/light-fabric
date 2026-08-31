import { createHmac, createHash, timingSafeEqual } from "node:crypto";
import { createServer } from "node:http";
import { readFileSync, renameSync, writeFileSync } from "node:fs";

export const CONTRACT_VERSION = "light-a2a-backend/v1";
const CONTEXT = "x-light-a2a-backend-context";
const SIGNATURE = "x-light-a2a-backend-signature";
const DIGEST = "x-light-a2a-backend-contract-digest";

class SecurityError extends Error {}

function reject(message) { throw new SecurityError(message); }

function consumeReplay(path, id, expires) {
  let values = {};
  try { values = JSON.parse(readFileSync(path, "utf8")); } catch (error) { if (error.code !== "ENOENT") throw error; }
  const now = Date.now();
  values = Object.fromEntries(Object.entries(values).filter(([, value]) => value > now));
  if (values[id]) return false;
  values[id] = expires;
  const temporary = `${path}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(values), { mode: 0o600 });
  renameSync(temporary, path);
  return true;
}

export function verify(headers, body, operation, config) {
  const encoded = headers[CONTEXT];
  const signature = headers[SIGNATURE];
  if (typeof encoded !== "string" || typeof signature !== "string" || headers[DIGEST] !== config.contractDigest) reject("missing or mismatched signed invocation headers");
  const actual = createHmac("sha256", config.key).update(encoded).update(Buffer.from([0])).update(body).digest();
  const supplied = Buffer.from(signature, "hex");
  if (supplied.length !== actual.length || !timingSafeEqual(actual, supplied)) reject("signature mismatch");
  const context = JSON.parse(Buffer.from(encoded, "base64url").toString("utf8"));
  const request = JSON.parse(body.toString("utf8"));
  const contextFields = ["audience","backendOperationId","bindingId","budget","callerAgentRef","contextId","contractVersion","dataBoundaryDigest","deadline","environment","expiresAt","hostId","idempotencyKey","invocationId","issuedAt","issuer","operation","policyDigest","principalSubject","publicationId","requestDigest","selectedSkillId","targetAgentRef","taskId","traceparent"];
  const requestFields = ["contextId","idempotencyKey","message","metadata","skillId","taskId"];
  if (!context || typeof context !== "object" || JSON.stringify(Object.keys(context).sort()) !== JSON.stringify(contextFields)
      || !request || typeof request !== "object" || JSON.stringify(Object.keys(request).sort()) !== JSON.stringify(requestFields)) reject("unknown or missing contract field");
  const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
  if (["invocationId","hostId","bindingId","publicationId","taskId","contextId"].some(key => typeof context[key] !== "string" || !uuid.test(context[key]))) reject("invalid UUID identity");
  const fixed = { contractVersion: CONTRACT_VERSION, issuer: "light-a2a", audience: config.audience, hostId: config.hostId, environment: config.environment, targetAgentRef: config.targetAgentRef, bindingId: config.bindingId, publicationId: config.publicationId, policyDigest: config.policyDigest, dataBoundaryDigest: config.dataBoundaryDigest, operation };
  for (const [key, value] of Object.entries(fixed)) if (context[key] !== value) reject(`invocation ${key} mismatch`);
  const issued = Date.parse(context.issuedAt); const deadline = Date.parse(context.deadline); const expires = Date.parse(context.expiresAt); const now = Date.now();
  const digest = value => typeof value === "string" && /^sha256:[0-9a-f]{64}$/.test(value);
  const budget = context.budget;
  if (![issued, deadline, expires].every(Number.isFinite) || issued > now + 30000 || deadline <= now || expires <= now || expires > deadline || expires > issued + 300000
      || ["principalSubject", "callerAgentRef", "targetAgentRef", "environment", "idempotencyKey"].some(key => typeof context[key] !== "string" || context[key].trim() === "")
      || ["policyDigest", "dataBoundaryDigest", "requestDigest"].some(key => !digest(context[key]))
      || !budget || ["maximumInputBytes", "maximumOutputBytes", "maximumArtifactBytes"].some(key => !Number.isSafeInteger(budget[key]) || budget[key] < 1)) reject("invalid invocation envelope");
  if (context.requestDigest !== `sha256:${createHash("sha256").update(body).digest("hex")}`) reject("request digest mismatch");
  for (const key of ["taskId", "contextId", "idempotencyKey"]) if (request[key] !== context[key]) reject(`unsigned ${key}`);
  if (request.skillId !== context.selectedSkillId) reject("unsigned skillId");
  if ((operation === "STATUS" || operation === "CANCEL") && !context.backendOperationId) reject("missing backend operation identity");
  if (!consumeReplay(config.replayFile, context.invocationId, expires)) reject("replayed invocation");
  return context;
}

export function serve(backend, config, port) {
  if (!Buffer.isBuffer(config.key) || config.key.length < 32 || !Number.isInteger(port) || port < 1) throw new Error("invalid adapter configuration");
  const maximum = config.maximumRequestBytes ?? 1_048_576;
  const operations = { "/v1/invoke": "INVOKE", "/v1/invoke-stream": "INVOKE_STREAM", "/v1/status": "STATUS", "/v1/cancel": "CANCEL" };
  return createServer(async (request, response) => {
    const send = (status, value) => { const body = JSON.stringify(value); response.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body) }); response.end(body); };
    if (request.method === "GET" && request.url === "/v1/capabilities") {
      if (request.headers[DIGEST] !== config.contractDigest) return send(401, { code: "BACKEND_INVOCATION_REJECTED", message: "contract digest mismatch", retryable: false });
      return send(200, backend.capabilities());
    }
    if (request.method === "GET" && ["/health/live", "/health/ready"].includes(request.url)) { response.writeHead(204); return response.end(); }
    const operation = request.method === "POST" ? operations[request.url] : undefined;
    if (!operation) return send(404, { code: "NOT_FOUND", message: "fixed route not found", retryable: false });
    const chunks = []; let size = 0;
    try {
      for await (const chunk of request) { size += chunk.length; if (size > maximum) throw new Error("request exceeded limit"); chunks.push(chunk); }
      const body = Buffer.concat(chunks); const context = verify(request.headers, body, operation, config); const businessRequest = JSON.parse(body);
      if (operation === "INVOKE_STREAM") {
        response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" }); let previous = 0; let terminal = false;
        for await (const event of backend.invokeStream(context, businessRequest)) { if (event.sequenceNumber <= previous) throw new Error("events are not ordered"); previous = event.sequenceNumber; terminal = event.terminal === true; response.write(`data: ${JSON.stringify(event)}\n\n`); }
        if (!terminal) throw new Error("stream did not terminate"); return response.end();
      }
      const callback = { INVOKE: "invoke", STATUS: "status", CANCEL: "cancel" }[operation]; return send(200, await backend[callback](context, businessRequest));
    } catch (error) { const security = error instanceof SecurityError; return send(security ? 401 : 422, { code: security ? "BACKEND_INVOCATION_REJECTED" : "BUSINESS_ERROR", message: String(error.message), retryable: false }); }
  }).listen(port, "127.0.0.1");
}
