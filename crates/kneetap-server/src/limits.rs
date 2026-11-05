use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, Sleep};

/// The bytes that begin every HTTP/2 connection (RFC 9113 section 3.4).
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Timeouts and capacity that keep one client from holding resources indefinitely.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_connections: usize,
    pub max_inflight_requests: usize,
    pub handshake_timeout: Duration,
    pub header_timeout: Duration,
    pub body_timeout: Duration,
    pub write_idle_timeout: Duration,
    pub idle_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_inflight_requests: 1024,
            handshake_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            write_idle_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

/// A gRPC request body that ends in an error once it goes `timeout` without a frame.
///
/// A streaming call can respond before its request ends, so reading can outlive the handler.
/// Without this, a client could hold a request permit by leaving its stream open. Each frame
/// restarts the wait, so a stream that keeps sending can run longer than `timeout`.
pub struct DeadlineBody {
    body: Body,
    timeout: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl DeadlineBody {
    pub fn new(body: Body, timeout: Duration) -> Self {
        Self {
            body,
            timeout,
            deadline: Box::pin(tokio::time::sleep(timeout)),
        }
    }
}

impl http_body::Body for DeadlineBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Poll::Ready(frame) = Pin::new(&mut self.body).poll_frame(cx) {
            let timeout = self.timeout;
            self.deadline.set(tokio::time::sleep(timeout));
            return Poll::Ready(frame);
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            let timed_out = io::Error::from(io::ErrorKind::TimedOut);
            return Poll::Ready(Some(Err(axum::Error::new(timed_out))));
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// Allows one event per interval, which limits how often a flood can block on a full log.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Mutex<Option<Instant>>,
}

impl Throttle {
    /// Returns whether an event may be logged now.
    pub fn allow(&self, interval: Duration) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|last| last.elapsed() < interval) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

/// Tracks whether a connection has requests in progress and when it last did.
///
/// HTTP/2 keep-alive pings prove a client is alive, not that it needs the connection, so this is
/// what lets an idle client be closed.
#[derive(Debug)]
pub struct Activity {
    active: AtomicUsize,
    last: Mutex<Instant>,
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        })
    }

    /// Marks a request as started until the returned guard is dropped.
    pub fn begin(self: &Arc<Self>) -> RequestGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        RequestGuard(self.clone())
    }

    /// Reads the last activity. A panic can't leave an `Instant` half-written, so poison is
    /// ignored.
    fn last(&self) -> Instant {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Completes once the connection has had no requests for `timeout`.
    ///
    /// This measures elapsed time rather than adding to an `Instant`, which a very long timeout
    /// could overflow.
    pub async fn idle_for(&self, timeout: Duration) {
        loop {
            let elapsed = self.last().elapsed();
            if elapsed >= timeout && self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            let remaining = timeout.saturating_sub(elapsed);
            let wait = if remaining.is_zero() {
                timeout
            } else {
                remaining
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// Keeps a connection from counting as idle.
#[derive(Debug)]
pub struct RequestGuard(Arc<Activity>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        *self.0.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A response body that holds its request's capacity until it finishes or the client disconnects.
///
/// Holding it here rather than in the handler means a slow download still counts as in progress.
pub struct TrackedBody {
    body: Body,
    _permit: OwnedSemaphorePermit,
    _guard: RequestGuard,
}

impl TrackedBody {
    pub const fn new(body: Body, permit: OwnedSemaphorePermit, guard: RequestGuard) -> Self {
        Self {
            body,
            _permit: permit,
            _guard: guard,
        }
    }
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// Bounds transport waits that the HTTP layers can't see.
///
/// The first deadline closes connections that stall before hyper has chosen a protocol, which it
/// would otherwise wait on forever. That covers silence and a partial HTTP/2 preface. Only blocked
/// writes run a timer, so slow but progressing downloads stay valid.
///
/// It also owns the connection's capacity, so an upgraded WebSocket keeps counting against the
/// limit after HTTP has finished with it.
pub struct Deadlines<I> {
    _permit: OwnedSemaphorePermit,
    inner: I,
    first_read: Option<Pin<Box<Sleep>>>,

    /// How many bytes so far match the HTTP/2 preface, which is what detection waits for.
    preface_matched: usize,
    write_timeout: Duration,
    write_timer: Option<Pin<Box<Sleep>>>,
}

impl<I> Deadlines<I> {
    pub fn new(inner: I, permit: OwnedSemaphorePermit, limits: &Limits) -> Self {
        Self {
            _permit: permit,
            inner,
            first_read: Some(Box::pin(tokio::time::sleep(limits.header_timeout))),
            preface_matched: 0,
            write_timeout: limits.write_idle_timeout,
            write_timer: None,
        }
    }

    /// Returns whether these bytes settle the protocol: they either leave the HTTP/2 preface or
    /// complete it.
    fn detection_finished(&mut self, received: &[u8]) -> bool {
        let expected = &H2_PREFACE[self.preface_matched..];
        let compared = received.len().min(expected.len());
        if received[..compared] != expected[..compared] {
            return true;
        }
        self.preface_matched += compared;
        self.preface_matched == H2_PREFACE.len()
    }

    /// Returns a timeout error once a blocked write has waited too long, starting the timer if
    /// this is the first wait.
    fn poll_write_timer(&mut self, cx: &mut Context<'_>) -> Option<io::Error> {
        let timeout = self.write_timeout;
        self.write_timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)))
            .as_mut()
            .poll(cx)
            .is_ready()
            .then(|| io::ErrorKind::TimedOut.into())
    }

    /// Applies the write timer to the result of a write or flush.
    fn track_write<T>(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if result.is_pending() {
            if let Some(error) = self.poll_write_timer(cx) {
                return Poll::Ready(Err(error));
            }
        } else {
            self.write_timer = None;
        }
        result
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Deadlines<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if result.is_pending() {
            if let Some(timer) = &mut self.first_read
                && timer.as_mut().poll(cx).is_ready()
            {
                return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
            }
        } else if self.first_read.is_some() {
            let received = &buffer.filled()[filled..];
            if self.detection_finished(received) {
                self.first_read = None;
            }
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Deadlines<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        self.track_write(cx, result)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bytes);
        self.track_write(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.track_write(cx, result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.track_write(cx, result)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn a_very_long_idle_timeout_does_not_panic() {
        let activity = Activity::new();

        let idle = activity.idle_for(Duration::MAX);
        let finished = tokio::time::timeout(Duration::from_millis(10), idle).await;

        assert!(finished.is_err(), "went idle immediately");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_request_stream_ends_at_its_deadline() {
        let stalled = futures_util::stream::pending::<Result<Bytes, io::Error>>();
        let body = Body::new(DeadlineBody::new(Body::from_stream(stalled), TIMEOUT));

        let read = axum::body::to_bytes(body, 1024).await;

        assert!(read.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_stream_that_keeps_sending_outlives_its_deadline() {
        let frames = futures_util::stream::iter(0..3).then(|_| async {
            tokio::time::sleep(TIMEOUT / 2).await;
            Ok::<_, io::Error>(Bytes::from_static(b"x"))
        });
        let body = Body::new(DeadlineBody::new(Body::from_stream(frames), TIMEOUT));
        let start = Instant::now();

        let read = axum::body::to_bytes(body, 1024).await.unwrap();

        assert_eq!((read.len(), start.elapsed()), (3, TIMEOUT * 3 / 2));
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_events_are_throttled_per_interval() {
        let throttle = Throttle::default();

        let allowed = [throttle.allow(TIMEOUT), throttle.allow(TIMEOUT)];
        tokio::time::advance(TIMEOUT).await;

        assert_eq!(allowed, [true, false]);
        assert!(throttle.allow(TIMEOUT));
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_with_no_requests_goes_idle() {
        let activity = Activity::new();
        let start = Instant::now();

        activity.idle_for(TIMEOUT).await;

        assert_eq!(start.elapsed(), TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_in_progress_prevents_idling() {
        let activity = Activity::new();
        let guard = activity.begin();

        let idle = tokio::time::timeout(TIMEOUT * 5, activity.idle_for(TIMEOUT)).await;

        assert!(idle.is_err(), "went idle during a request");
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_counts_from_the_end_of_the_last_request() {
        let activity = Activity::new();
        tokio::time::advance(TIMEOUT / 2).await;
        drop(activity.begin());
        let start = Instant::now();

        activity.idle_for(TIMEOUT).await;

        assert_eq!(start.elapsed(), TIMEOUT);
    }
}
