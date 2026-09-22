//! Light CLI: sign the user in, keep that login alive, and call `light-gateway`.
//!
//! The CLI is open source and downloadable anywhere, so it is a **public client**: nothing in
//! it is a secret, and it has no certificate. It does carry a dev **application token**, a public
//! identifier for the platform services that ask which application is calling (the config server
//! today), which is never sent to the Gateway or light-oauth. `/login` signs the user in with the OAuth
//! device grant against light-oauth (through the Gateway); every Gateway call carries the user's
//! access token, refreshed on use. What a caller may do is decided by the user's own roles.
//!
//! The design is `docs/src/design/light-cli.md`, and for the grant
//! `light-portal-doc/src/design/light-oauth/device-authorization.md`.

pub mod auth;
pub mod chat;
pub mod config;
pub mod error;
pub mod gateway;
pub mod http;
pub mod oauth;
pub mod output;
pub mod remote;
pub mod session;
pub mod shell;
pub mod store;
pub mod terminal;
pub mod text;
