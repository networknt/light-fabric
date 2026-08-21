# Light Knowledge Administration

Private, user-delegated operational API used by `genai-query`. This is
a separate application and listener from `light-knowledge`; the public
retrieval service does not mount administration routes.

`genai-query` forwards the authenticated Portal bearer token unchanged. The
service independently verifies its signature, issuer/audience, `portal.r`
scope, Knowledge-administrator role, and host claim. It derives global access
only from `admin` or `platformKnowledgeBaseAdmin`; every query also applies the
host/environment predicate in the Knowledge database. The environment header
is set by `genai-query` on the private listener and is not accepted from a
public route.

All collection queries have fixed relation/column definitions, composite
server-signed cursors, a maximum page size of 200, per-field JSON limits, and a
1 MiB response limit. Raw principal identifiers and locators are not returned.

Every operational read is also gated by the latest non-expired applied control
snapshot for the delegated token's host and requested environment. Cursors are
bound to that Knowledge Base, environment, resource, and optional generation;
authorization and snapshot freshness are evaluated again on every page.

The `/metrics` endpoint exposes request, latency histogram, result, denial,
redaction, timeout, and database-pool measurements. Route templates are the
only labels; host, user, tenant, and Knowledge Base identifiers are never metric
labels. Migration estimates and authorization simulations additionally write a
content-safe digest audit row and do not mutate operational state.

The Kubernetes resources deliberately define a distinct Deployment, private
ClusterIP Service, service identity, health checks, database pool, and replica
count. Scale this Deployment independently from the public `light-knowledge`
retrieval Deployment; do not publish its Service through public ingress.
