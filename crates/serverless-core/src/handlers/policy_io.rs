//! Lambda header I/O around the shared neutral policy traits.
use crate::HandlerFailure;
use gateway_policy::{HeaderWriter, Request, RequestHead, RequestRead};
use http::{HeaderMap, HeaderValue, Method, header::HeaderName};

pub struct PolicyHeaders<'a> {
    pub headers: &'a mut HeaderMap,
    pub method: &'a Method,
}
impl HeaderWriter for PolicyHeaders<'_> {
    type Error = HandlerFailure;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<(), Self::Error> {
        let name = HeaderName::from_bytes(name.as_ref().as_bytes()).map_err(|_| HandlerFailure)?;
        let value = HeaderValue::from_str(value.as_ref()).map_err(|_| HandlerFailure)?;
        self.headers.insert(name, value);
        Ok(())
    }
    fn remove_header(&mut self, name: &str) {
        self.headers.remove(name);
    }
}
impl RequestRead for PolicyHeaders<'_> {
    fn req_header(&self) -> RequestHead<'_> {
        RequestHead {
            headers: self.headers,
            method: self.method,
        }
    }
}
impl Request for PolicyHeaders<'_> {
    type Error = HandlerFailure;
    type Writer = Self;
    fn req_header_mut(&mut self) -> &mut Self {
        self
    }
}

/// Staged proof for the real request Header policy. The normalized transport
/// map is unchanged; only successful mutations update the private candidate.
pub struct TrackedHeaders<'a> {
    pub inner: PolicyHeaders<'a>,
    pub media: &'a mut crate::ValidatorMediaProof,
}
impl HeaderWriter for TrackedHeaders<'_> {
    type Error = HandlerFailure;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<(), HandlerFailure> {
        let raw_name = name.as_ref();
        let key = HeaderName::from_bytes(raw_name.as_bytes()).map_err(|_| HandlerFailure)?;
        let value = HeaderValue::from_str(value.as_ref()).map_err(|_| HandlerFailure)?;
        self.media
            .record_insert(raw_name, &value, self.inner.headers.contains_key(&key));
        self.inner.headers.insert(key, value);
        Ok(())
    }
    fn remove_header(&mut self, name: &str) {
        self.inner.headers.remove(name);
        self.media.record_remove(name);
    }
}
impl RequestRead for TrackedHeaders<'_> {
    fn req_header(&self) -> RequestHead<'_> {
        self.inner.req_header()
    }
}
impl Request for TrackedHeaders<'_> {
    type Error = HandlerFailure;
    type Writer = Self;
    fn req_header_mut(&mut self) -> &mut Self {
        self
    }
}
