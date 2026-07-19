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
    }
    session.write_response_body(None, true).await
}
