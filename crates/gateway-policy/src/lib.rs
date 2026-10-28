//! Transport-neutral gateway policy decisions. HTTP I/O stays in adapters.
pub mod correlation;

use http::{HeaderMap, Method};

/// A borrowed HTTP head. No transport session or lifecycle is exposed.
pub struct RequestHead<'a> {
    pub headers: &'a HeaderMap,
    pub method: &'a Method,
}

/// Ordered header operations are performed by the adapter so its validation,
/// original casing, partial mutation and error mapping remain unchanged.
pub trait HeaderWriter {
    type Error;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<(), Self::Error>;
    fn remove_header(&mut self, name: &str);
}

pub trait RequestRead {
    fn req_header(&self) -> RequestHead<'_>;
}

pub trait Request: RequestRead {
    type Error;
    type Writer: HeaderWriter<Error = Self::Error>;
    fn req_header_mut(&mut self) -> &mut Self::Writer;
}

mod config_util;
pub mod header;

pub mod cors;
pub mod java_cors;

pub mod apikey;

pub mod security;

pub mod basic_auth;

pub mod rate_limit;

pub mod audit;
pub mod audit_mask;
pub mod audit_timestamp;
#[doc(hidden)]
pub mod direct_registry;
pub mod metrics;
pub mod observation;
pub mod openapi_operation;
pub mod sanitizer_body;
pub mod sanitizer_encoding;
mod sanitizer_extra_tables;
mod sanitizer_tables;
pub mod token;
pub mod unified_security;
