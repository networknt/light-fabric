# Changelog

## 0.2.0 - 2026-07-29

- Complete the SPA session-endpoint POST migration: MSAL exchange and all
  logout handlers now reject legacy `GET` with `405`/`ERR10008` and
  `Allow: POST` before authentication, cookie mutation, default-handler
  fallback, or upstream routing. OAuth callbacks remain `GET`-only, rejected
  legacy calls retain queryable telemetry, and configured CORS headers remain
  visible on cross-origin method rejections.
- Normalize Rust MSAL and stateless-auth logout responses to `204 No Content`
  without representation headers and delete every cookie owned by each runtime.
- Add endpoint-separated logout CSRF would-reject telemetry and an observe-only
  `logoutCsrfEnforced` promotion switch for MSAL exchange, MSAL auth, and
  stateless auth. The shipped default remains `false`.
- Change the public `SpaAuthResponse.content_type` field to `Option<String>`.
  This workspace-wide crate version does not replace the independently
  versioned light-gateway deployment image tag; include these notes in the
  next image release record.
