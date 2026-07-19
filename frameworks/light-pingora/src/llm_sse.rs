use llm_gateway::runtime::LlmStreamExecution;
use pingora::http::ResponseHeader;
use pingora::prelude::Session;
use pingora::{Error, ErrorType};

/// Writes the LF-6B bounded SSE stream. Dropping the stream on any downstream
/// error cancels the upstream provider and releases all stream-owned permits.
pub async fn write_llm_sse_response(
    session: &mut Session,
    header: ResponseHeader,
    mut stream: LlmStreamExecution,
) -> pingora::Result<()> {
    session
        .write_response_header(Box::new(header), false)
        .await?;
    while let Some(frame) = stream.next_frame().await {
        let frame_bytes = frame.len() as u128;
        let write_started = std::time::Instant::now();
        match tokio::time::timeout(
            stream.write_timeout,
            session.write_response_body(Some(frame), false),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                stream.cancel();
                return Err(Error::explain(
                    ErrorType::WriteError,
                    "LLM SSE downstream write deadline exceeded",
                ));
            }
        }
        let write_elapsed = write_started.elapsed();
        if below_minimum_drain_rate(
            frame_bytes,
            write_elapsed,
            stream.drain_grace,
            stream.minimum_drain_bytes_per_second,
        ) {
            stream.cancel();
            return Err(Error::explain(
                ErrorType::WriteError,
                "LLM SSE downstream drain rate is below the configured minimum",
            ));
        }
    }
    session.write_response_body(None, true).await
}

fn below_minimum_drain_rate(
    frame_bytes: u128,
    elapsed: std::time::Duration,
    grace: std::time::Duration,
    minimum_bytes_per_second: u64,
) -> bool {
    elapsed >= grace
        && frame_bytes.saturating_mul(1_000_000_000)
            < (minimum_bytes_per_second as u128).saturating_mul(elapsed.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::below_minimum_drain_rate;
    use std::time::Duration;

    #[test]
    fn downstream_drain_rate_enforces_grace_and_threshold() {
        assert!(!below_minimum_drain_rate(
            1,
            Duration::from_millis(999),
            Duration::from_secs(1),
            128,
        ));
        assert!(below_minimum_drain_rate(
            64,
            Duration::from_secs(1),
            Duration::from_secs(1),
            128,
        ));
        assert!(!below_minimum_drain_rate(
            128,
            Duration::from_secs(1),
            Duration::from_secs(1),
            128,
        ));
    }
}
