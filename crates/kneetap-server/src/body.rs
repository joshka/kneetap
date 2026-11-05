use std::error::Error;
use std::time::Duration;

use axum::body::{self, Body, Bytes};
use axum::http::StatusCode;
use http_body_util::LengthLimitError;

use crate::routes::Rejection;

/// Reads a request body, refusing one that's too large or arrives too slowly.
pub async fn read(body: Body, limit: usize, timeout: Duration) -> Result<Bytes, Rejection> {
    let read = tokio::time::timeout(timeout, body::to_bytes(body, limit)).await;
    let bytes = read.map_err(|_| (StatusCode::REQUEST_TIMEOUT, "body read timed out\n"))?;
    bytes.map_err(|error| rejection(&error))
}

/// Tells a body over the limit, which the client can fix, from one that failed to arrive.
fn rejection(error: &axum::Error) -> Rejection {
    let source = error.source();
    if source.is_some_and(<dyn Error>::is::<LengthLimitError>) {
        (StatusCode::PAYLOAD_TOO_LARGE, "body exceeds maximum\n")
    } else {
        (StatusCode::BAD_REQUEST, "body read failed\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn a_body_at_the_limit_is_read() {
        let bytes = read("1234".into(), 4, TIMEOUT).await.unwrap();

        assert_eq!(bytes, "1234");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_too_large() {
        let error = read("12345".into(), 4, TIMEOUT).await.unwrap_err();

        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_never_finishes_times_out() {
        let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();

        let error = read(Body::from_stream(stalled), 4, TIMEOUT)
            .await
            .unwrap_err();

        assert_eq!(error.0, StatusCode::REQUEST_TIMEOUT);
    }
}
