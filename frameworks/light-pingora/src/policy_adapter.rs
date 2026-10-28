use gateway_policy::{HeaderWriter, Request, RequestHead, RequestRead};
use pingora::{http::ResponseHeader, prelude::Session};

pub(crate) struct PolicyRequest<'a>(pub(crate) &'a mut Session);

impl HeaderWriter for PolicyRequest<'_> {
    type Error = Box<pingora::Error>;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> pingora::Result<()> {
        self.0
            .req_header_mut()
            .insert_header(name.as_ref().to_string(), value.as_ref())
    }
    fn remove_header(&mut self, name: &str) {
        self.0.req_header_mut().remove_header(name);
    }
}

impl Request for PolicyRequest<'_> {
    type Error = Box<pingora::Error>;
    type Writer = Self;
    fn req_header_mut(&mut self) -> &mut Self::Writer {
        self
    }
}

impl RequestRead for PolicyRequest<'_> {
    fn req_header(&self) -> RequestHead<'_> {
        RequestHead {
            headers: &self.0.req_header().headers,
            method: &self.0.req_header().method,
        }
    }
}

pub(crate) struct PolicyRequestRef<'a>(pub(crate) &'a Session);
impl RequestRead for PolicyRequestRef<'_> {
    fn req_header(&self) -> RequestHead<'_> {
        RequestHead {
            headers: &self.0.req_header().headers,
            method: &self.0.req_header().method,
        }
    }
}

pub(crate) struct PolicyResponse<'a>(pub(crate) &'a mut ResponseHeader);

impl HeaderWriter for PolicyResponse<'_> {
    type Error = Box<pingora::Error>;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> pingora::Result<()> {
        self.0
            .insert_header(name.as_ref().to_string(), value.as_ref())
    }
    fn remove_header(&mut self, name: &str) {
        self.0.remove_header(name);
    }
}
