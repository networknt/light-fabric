# Changelog

## 0.2.0 - 2026-07-29

- Add the SPA session-endpoint compatibility bridge: `GET` and `POST` reach
  MSAL exchange and all logout handlers during migration, unsupported methods
  fail before proxying, OAuth callbacks remain `GET`-only, and every legacy
  `GET` emits queryable structured telemetry plus bounded checkpoint warnings.
- Normalize Rust MSAL and stateless-auth logout responses to `204 No Content`
  without representation headers and delete every cookie owned by each runtime.
- Add endpoint-separated logout CSRF would-reject telemetry and an observe-only
  `logoutCsrfEnforced` promotion switch for MSAL exchange, MSAL auth, and
  stateless auth. The shipped default remains `false`.
- Change the public `SpaAuthResponse.content_type` field to `Option<String>`.
  This workspace-wide crate version does not replace the independently
  versioned light-gateway deployment image tag; include these notes in the
  next image release record.
