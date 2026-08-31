# Phase 6 Threat And Rollback Contract

| Threat | Required control |
| --- | --- |
| Extended-card disclosure | Authenticate and authorize before conditional-cache evaluation; bind the ETag to policy digest and revocation epoch. |
| Extension confusion | A2A 1.0 only; exact URI/schema/operation handler match; optional data-only; response echoes only activated extensions. |
| Callback SSRF or rebinding | Portal-approved fixed HTTPS registration, global-address validation at publication and delivery, DNS re-resolution, no redirects. |
| Callback credential injection | Ignore caller credentials; load an HMAC key from a server-owned secret file. |
| Replay | Unique persisted delivery ID and nonce, signed timestamp and payload digest; receiving endpoint applies its replay window. |
| Lost or duplicate delivery | Durable outbox, bounded attempts, leases, idempotent delivery ID, and terminal dead-letter state. |
| Cross-tenant task access | Bind task, callback registration, binding, and owning principal in operational rows and recheck them for every configuration method. |

Rollback is a snapshot publication that removes the Phase 6 selectors. It does
not silently fall back to caller-provided URLs, unsigned cards, required
extensions, A2A 0.3, or a different transport.

