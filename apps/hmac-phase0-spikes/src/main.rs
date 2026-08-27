use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pingora::apps::HttpServerOptions;
use pingora::prelude::{Error, ErrorType, HttpPeer, Result, Session};
use pingora::proxy::{ProxyHttp, ProxyServiceBuilder};
use pingora::server::Server;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_BODY_TIMEOUT_MILLIS: u64 = 10_000;

#[derive(Debug, Default)]
struct BodyGateContext {
    captured: Option<Bytes>,
    injected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureOutcome {
    Complete,
    TooLarge,
}

struct Phase0BodyGateProxy {
    upstream: SocketAddr,
    max_body_bytes: usize,
    body_timeout: Duration,
}

impl Phase0BodyGateProxy {
    async fn capture_body(
        &self,
        session: &mut Session,
        output: &mut BytesMut,
    ) -> Result<CaptureOutcome> {
        loop {
            let Some(chunk) = session.read_request_body().await? else {
                return Ok(CaptureOutcome::Complete);
            };
            let remaining = self.max_body_bytes.saturating_sub(output.len());
            if chunk.len() > remaining {
                return Ok(CaptureOutcome::TooLarge);
            }
            output.extend_from_slice(&chunk);
        }
    }

    async fn respond_empty(session: &mut Session, status: u16) -> Result<bool> {
        session.respond_error(status).await?;
        Ok(true)
    }
}

#[async_trait]
impl ProxyHttp for Phase0BodyGateProxy {
    type CTX = BodyGateContext;

    fn new_ctx(&self) -> Self::CTX {
        BodyGateContext::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        if let Some(content_length) = session
            .req_header()
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            && content_length > self.max_body_bytes
        {
            return Self::respond_empty(session, 413).await;
        }

        let mut captured = BytesMut::new();
        let capture = timeout(self.body_timeout, self.capture_body(session, &mut captured)).await;
        match capture {
            Err(_) => return Self::respond_empty(session, 408).await,
            Ok(Err(error)) => return Err(error),
            Ok(Ok(CaptureOutcome::TooLarge)) => {
                return Self::respond_empty(session, 413).await;
            }
            Ok(Ok(CaptureOutcome::Complete)) => {}
        }

        if session.req_header().uri.path() == "/duplicate" {
            return Self::respond_empty(session, 200).await;
        }

        ctx.captured = Some(captured.freeze());
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(self.upstream, false, String::new())))
    }

    fn prebuffered_request_body(&self, _session: &Session, ctx: &Self::CTX) -> Option<Bytes> {
        ctx.captured.clone()
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx.injected {
            return Err(Error::explain(
                ErrorType::InternalError,
                "Phase 0 body gate was invoked after verified-body reinjection",
            ));
        }
        if !end_of_stream {
            return Err(Error::explain(
                ErrorType::InternalError,
                "Phase 0 body gate did not consume the downstream body before upstream selection",
            ));
        }
        let captured = ctx.captured.take().ok_or_else(|| {
            Error::explain(
                ErrorType::InternalError,
                "Phase 0 body gate reached upstream without captured bytes",
            )
        })?;
        *body = Some(captured);
        ctx.injected = true;
        Ok(())
    }
}

fn parse_args() -> std::result::Result<(SocketAddr, SocketAddr, usize, Duration), String> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: hmac-phase0-spikes <listen-address> <upstream-address> [max-body-bytes] [body-timeout-millis]";
    let listen = args
        .next()
        .ok_or(usage)?
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid listen address: {error}"))?;
    let upstream = args
        .next()
        .ok_or(usage)?
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid upstream address: {error}"))?;
    let max_body_bytes = args
        .next()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|error| format!("invalid max body bytes: {error}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_BODY_BYTES);
    let body_timeout_millis = args
        .next()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| format!("invalid body timeout: {error}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_BODY_TIMEOUT_MILLIS);
    if max_body_bytes == 0 || body_timeout_millis == 0 || args.next().is_some() {
        return Err(usage.to_string());
    }
    Ok((
        listen,
        upstream,
        max_body_bytes,
        Duration::from_millis(body_timeout_millis),
    ))
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (listen, upstream, max_body_bytes, body_timeout) = parse_args()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let mut server = Server::new(None)?;
    server.bootstrap();
    let mut options = HttpServerOptions::default();
    options.h2c = true;
    let mut proxy = ProxyServiceBuilder::new(
        &server.configuration,
        Phase0BodyGateProxy {
            upstream,
            max_body_bytes,
            body_timeout,
        },
    )
    .name("HMAC Phase 0 Body Gate")
    .server_options(options)
    .build();
    proxy.add_tcp(listen.to_string().as_str());
    server.add_service(proxy);
    server.run_forever();
}
