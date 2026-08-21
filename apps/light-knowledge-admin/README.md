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
