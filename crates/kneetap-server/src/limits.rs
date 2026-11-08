use std::collections::BTreeSet;
use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};

/// The bytes that begin every HTTP/2 connection (RFC 9113 section 3.4).
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The length of the header before each HTTP/2 frame's payload (RFC 9113 section 4.1).
const H2_FRAME_HEADER_SIZE: usize = 9;
const H2_HEADERS: u8 = 0x1;
const H2_CONTINUATION: u8 = 0x9;
const H2_END_HEADERS: u8 = 0x4;

/// The gRPC status for a call a client can retry elsewhere or later.
const GRPC_UNAVAILABLE: &str = "14";

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
/// restarts the wait, so a stream that keeps sending can run longer than `timeout`. It also ends
/// when `ended` is cancelled, because a handler still reading would otherwise keep the stream open
/// after its response has finished.
pub struct DeadlineBody {
    body: Body,
    timeout: Duration,
    deadline: Pin<Box<Sleep>>,
    ended: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl DeadlineBody {
    pub fn new(body: Body, timeout: Duration, ended: CancellationToken) -> Self {
        Self {
            body,
            timeout,
            deadline: Box::pin(tokio::time::sleep(timeout)),
            ended: Box::pin(ended.cancelled_owned()),
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
        if self.deadline.as_mut().poll(cx).is_ready() || self.ended.as_mut().poll(cx).is_ready() {
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

/// A gRPC response body that ends a call once it goes quiet or the server stops.
///
/// A stream such as a health watch can wait forever for something to send, holding its request
/// permit, and would hold a graceful shutdown open too. Only a body with nothing to send ends, so
/// messages already available are delivered first. Clients see `UNAVAILABLE`, which tells them to
/// call again. Dropping this cancels `ended`, which also ends the call's request stream.
pub struct QuietBody {
    body: Body,
    timeout: Duration,
    idle: Pin<Box<Sleep>>,
    stop: Pin<Box<WaitForCancellationFutureOwned>>,
    _ended: DropGuard,
    finished: bool,
}

impl QuietBody {
    pub fn new(
        body: Body,
        timeout: Duration,
        stop: CancellationToken,
        ended: CancellationToken,
    ) -> Self {
        Self {
            body,
            timeout,
            idle: Box::pin(tokio::time::sleep(timeout)),
            stop: Box::pin(stop.cancelled_owned()),
            _ended: ended.drop_guard(),
            finished: false,
        }
    }

    /// Finishes the call with trailers carrying `message`, which must be percent-encoded.
    fn finish(&mut self, message: &'static str) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        self.finished = true;
        let mut trailers = HeaderMap::new();
        trailers.insert(
            HeaderName::from_static("grpc-status"),
            HeaderValue::from_static(GRPC_UNAVAILABLE),
        );
        trailers.insert(
            HeaderName::from_static("grpc-message"),
            HeaderValue::from_static(message),
        );
        Poll::Ready(Some(Ok(Frame::trailers(trailers))))
    }
}

impl http_body::Body for QuietBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Poll::Ready(frame) = Pin::new(&mut self.body).poll_frame(cx) {
            let timeout = self.timeout;
            self.idle.set(tokio::time::sleep(timeout));
            return Poll::Ready(frame);
        }
        if self.stop.as_mut().poll(cx).is_ready() {
            return self.finish("server%20shutting%20down");
        }
        if self.idle.as_mut().poll(cx).is_ready() {
            return self.finish("idle%20timeout");
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.finished || self.body.is_end_stream()
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

    /// When each response started waiting for the client to take more, oldest first.
    waits: Mutex<BTreeSet<(Instant, u64)>>,
    next_wait: AtomicU64,
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
            waits: Mutex::default(),
            next_wait: AtomicU64::new(0),
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

impl Activity {
    /// Starts tracking an HTTP/2 response, which begins by waiting for hyper to read its body.
    pub fn track_response(self: &Arc<Self>) -> ResponseWait {
        let mut wait = ResponseWait {
            activity: self.clone(),
            id: self.next_wait.fetch_add(1, Ordering::Relaxed),
            since: None,
        };
        wait.begin();
        wait
    }

    /// Returns when the longest current wait began. A panic can't leave the set half-updated, so
    /// poison is ignored.
    fn oldest_wait(&self) -> Option<Instant> {
        let waits = self.waits.lock().unwrap_or_else(PoisonError::into_inner);
        waits.first().map(|wait| wait.0)
    }

    /// Completes once a response has waited `timeout` for the client to accept more of it.
    pub async fn stalled_for(&self, timeout: Duration) {
        loop {
            let waited = self.oldest_wait().map(|since| since.elapsed());
            if waited.is_some_and(|waited| waited >= timeout) {
                return;
            }
            let remaining = timeout.saturating_sub(waited.unwrap_or_default());
            tokio::time::sleep(remaining).await;
        }
    }
}

/// Records when an HTTP/2 response has data to send but hyper isn't asking for it.
///
/// While a client's flow-control window is closed, hyper stops reading the response body without
/// any write blocking on the socket. Nothing polls the body, so it can't time itself out. The
/// connection's watcher sees the wait here instead. A body that has nothing to send isn't waiting
/// on the client, so a quiet stream doesn't count.
#[derive(Debug)]
pub struct ResponseWait {
    activity: Arc<Activity>,
    id: u64,
    since: Option<Instant>,
}

impl ResponseWait {
    /// Marks the response as waiting for hyper to ask for more.
    fn begin(&mut self) {
        let since = Instant::now();
        let waits = &self.activity.waits;
        let mut waits = waits.lock().unwrap_or_else(PoisonError::into_inner);
        waits.insert((since, self.id));
        self.since = Some(since);
    }

    /// Marks the response as no longer waiting, because hyper has asked for more.
    fn end(&mut self) {
        if let Some(since) = self.since.take() {
            let waits = &self.activity.waits;
            let mut waits = waits.lock().unwrap_or_else(PoisonError::into_inner);
            waits.remove(&(since, self.id));
        }
    }
}

impl Drop for ResponseWait {
    fn drop(&mut self) {
        self.end();
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
    _permit: Arc<OwnedSemaphorePermit>,
    _guard: RequestGuard,
    wait: Option<ResponseWait>,
}

impl TrackedBody {
    pub const fn new(
        body: Body,
        permit: Arc<OwnedSemaphorePermit>,
        guard: RequestGuard,
        wait: Option<ResponseWait>,
    ) -> Self {
        Self {
            body,
            _permit: permit,
            _guard: guard,
            wait,
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
        if let Some(wait) = &mut self.wait {
            wait.end();
        }
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        if let (Poll::Ready(Some(_)), Some(wait)) = (&frame, &mut self.wait) {
            wait.begin();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// Finds incomplete HTTP/2 header blocks in the bytes a client sends after the preface.
///
/// hyper only sees a request once its header block is complete, so a stalled block looks the same
/// to it as an idle connection. A block spans a HEADERS frame and any CONTINUATION frames, up to
/// `END_HEADERS` (RFC 9113 section 4.3).
#[derive(Debug, Default)]
struct HeaderBlocks {
    frame_header: [u8; H2_FRAME_HEADER_SIZE],
    frame_header_filled: usize,
    payload_remaining: usize,
    in_header_frame: bool,
    awaiting_continuation: bool,
}

impl HeaderBlocks {
    /// Follows frame boundaries through `bytes`, which may split a frame anywhere.
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.payload_remaining > 0 {
                let skipped = self.payload_remaining.min(bytes.len());
                self.payload_remaining -= skipped;
                bytes = &bytes[skipped..];
                continue;
            }
            let copied = (H2_FRAME_HEADER_SIZE - self.frame_header_filled).min(bytes.len());
            let filled = self.frame_header_filled + copied;
            self.frame_header[self.frame_header_filled..filled].copy_from_slice(&bytes[..copied]);
            self.frame_header_filled = filled;
            bytes = &bytes[copied..];
            if filled == H2_FRAME_HEADER_SIZE {
                self.begin_frame();
            }
        }
    }

    /// Reads the completed frame header: a 24-bit length, then the type and flags.
    fn begin_frame(&mut self) {
        let [high, middle, low, kind, flags, ..] = self.frame_header;
        self.frame_header_filled = 0;
        self.payload_remaining =
            usize::from(high) << 16 | usize::from(middle) << 8 | usize::from(low);
        self.in_header_frame = matches!(kind, H2_HEADERS | H2_CONTINUATION);
        if self.in_header_frame {
            self.awaiting_continuation = flags & H2_END_HEADERS == 0;
        }
    }

    /// Returns whether the client is partway through a header block, or a frame header that may
    /// start one.
    const fn is_open(&self) -> bool {
        self.frame_header_filled > 0
            || self.awaiting_continuation
            || (self.in_header_frame && self.payload_remaining > 0)
    }
}

/// What the bytes read so far show about the connection's protocol.
enum Protocol {
    /// Every byte so far, this many of them, matches the HTTP/2 preface.
    Detecting(usize),
    Http1,
    Http2(HeaderBlocks),
}

/// Bounds transport waits that the HTTP layers can't see.
///
/// The read deadline closes connections that stall before hyper has chosen a protocol, which it
/// would otherwise wait on forever. That covers silence and a partial HTTP/2 preface. It runs again
/// for each HTTP/2 header block, because hyper's header timeout only applies to HTTP/1. Otherwise
/// reads wait freely, so an idle connection stays governed by the idle timeout. Only blocked writes
/// run a timer, so slow but progressing downloads stay valid.
///
/// It also owns the connection's capacity, so an upgraded WebSocket keeps counting against the
/// limit after HTTP has finished with it.
pub struct Deadlines<I> {
    _permit: OwnedSemaphorePermit,
    inner: I,
    protocol: Protocol,
    header_timeout: Duration,
    read_deadline: Option<Pin<Box<Sleep>>>,
    write_timeout: Duration,
    write_timer: Option<Pin<Box<Sleep>>>,
}

impl<I> Deadlines<I> {
    pub fn new(inner: I, permit: OwnedSemaphorePermit, limits: &Limits) -> Self {
        Self {
            _permit: permit,
            inner,
            protocol: Protocol::Detecting(0),
            header_timeout: limits.header_timeout,
            read_deadline: Some(Box::pin(tokio::time::sleep(limits.header_timeout))),
            write_timeout: limits.write_idle_timeout,
            write_timer: None,
        }
    }

    /// Updates the read deadline for bytes the client just sent.
    fn observe(&mut self, received: &[u8]) {
        let frames = match &mut self.protocol {
            Protocol::Http1 => return,
            Protocol::Http2(_) => received,
            Protocol::Detecting(matched) => {
                let expected = &H2_PREFACE[*matched..];
                let compared = received.len().min(expected.len());
                if received[..compared] != expected[..compared] {
                    self.protocol = Protocol::Http1;
                    self.read_deadline = None;
                    return;
                }
                *matched += compared;
                if *matched < H2_PREFACE.len() {
                    return;
                }
                self.protocol = Protocol::Http2(HeaderBlocks::default());
                self.read_deadline = None;
                &received[compared..]
            }
        };
        let Protocol::Http2(blocks) = &mut self.protocol else {
            return;
        };
        blocks.feed(frames);
        if !blocks.is_open() {
            self.read_deadline = None;
        } else if self.read_deadline.is_none() {
            self.read_deadline = Some(Box::pin(tokio::time::sleep(self.header_timeout)));
        }
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
            if let Some(deadline) = &mut self.read_deadline
                && deadline.as_mut().poll(cx).is_ready()
            {
                return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
            }
        } else {
            self.observe(&buffer.filled()[filled..]);
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
    use http_body_util::BodyExt;
    use rstest::rstest;

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
        let ended = CancellationToken::new();
        let body = Body::new(DeadlineBody::new(
            Body::from_stream(stalled),
            TIMEOUT,
            ended,
        ));

        let read = axum::body::to_bytes(body, 1024).await;

        assert!(read.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_stream_that_keeps_sending_outlives_its_deadline() {
        let frames = futures_util::stream::iter(0..3).then(|_| async {
            tokio::time::sleep(TIMEOUT / 2).await;
            Ok::<_, io::Error>(Bytes::from_static(b"x"))
        });
        let ended = CancellationToken::new();
        let body = Body::new(DeadlineBody::new(Body::from_stream(frames), TIMEOUT, ended));
        let start = Instant::now();

        let read = axum::body::to_bytes(body, 1024).await.unwrap();

        assert_eq!((read.len(), start.elapsed()), (3, TIMEOUT * 3 / 2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_stream_ends_with_its_response() {
        let stalled = futures_util::stream::pending::<Result<Bytes, io::Error>>();
        let ended = CancellationToken::new();
        let body = DeadlineBody::new(Body::from_stream(stalled), TIMEOUT, ended.clone());
        let start = Instant::now();

        ended.cancel();
        let read = axum::body::to_bytes(Body::new(body), 1024).await;

        assert!(read.is_err());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// Reads a quiet stream's trailers, with the time they took to arrive.
    async fn quiet_trailers(stop: CancellationToken) -> (HeaderMap, Duration) {
        let quiet = futures_util::stream::pending::<Result<Bytes, io::Error>>();
        let ended = CancellationToken::new();
        let mut body = QuietBody::new(Body::from_stream(quiet), TIMEOUT, stop, ended);
        let start = Instant::now();

        let frame = body.frame().await.unwrap().unwrap();

        assert!(body.frame().await.is_none());
        (frame.into_trailers().unwrap(), start.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_stream_ends_as_unavailable_at_the_idle_timeout() {
        let (trailers, elapsed) = quiet_trailers(CancellationToken::new()).await;

        assert_eq!(trailers["grpc-status"], GRPC_UNAVAILABLE);
        assert_eq!(trailers["grpc-message"], "idle%20timeout");
        assert_eq!(elapsed, TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_stream_ends_at_once_when_the_server_stops() {
        let stop = CancellationToken::new();
        stop.cancel();

        let (trailers, elapsed) = quiet_trailers(stop).await;

        assert_eq!(trailers["grpc-message"], "server%20shutting%20down");
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_keeps_sending_is_not_quiet() {
        let frames = futures_util::stream::iter(0..3).then(|_| async {
            tokio::time::sleep(TIMEOUT / 2).await;
            Ok::<_, io::Error>(Bytes::from_static(b"x"))
        });
        let stop = CancellationToken::new();
        let ended = CancellationToken::new();
        let body = QuietBody::new(Body::from_stream(frames), TIMEOUT, stop, ended);

        let read = axum::body::to_bytes(Body::new(body), 1024).await.unwrap();

        assert_eq!(read.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_response_hyper_stops_reading_stalls_the_connection() {
        let activity = Activity::new();
        let _wait = activity.track_response();
        let start = Instant::now();

        activity.stalled_for(TIMEOUT).await;

        assert_eq!(start.elapsed(), TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_response_with_nothing_to_send_does_not_stall() {
        let activity = Activity::new();
        let mut wait = activity.track_response();

        wait.end();
        let stalled = tokio::time::timeout(TIMEOUT * 5, activity.stalled_for(TIMEOUT)).await;

        assert!(stalled.is_err(), "stalled without waiting on the client");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_response_stops_counting_as_stalled() {
        let activity = Activity::new();

        drop(activity.track_response());
        let stalled = tokio::time::timeout(TIMEOUT * 5, activity.stalled_for(TIMEOUT)).await;

        assert!(stalled.is_err(), "a dropped response stalled");
    }

    /// Builds a frame header for a payload of `length` bytes.
    fn frame_header(length: u8, kind: u8, flags: u8) -> [u8; H2_FRAME_HEADER_SIZE] {
        [0, 0, length, kind, flags, 0, 0, 0, 1]
    }

    const SETTINGS: u8 = 0x4;
    const DATA: u8 = 0x0;

    #[rstest]
    #[case::nothing_yet(&[], false)]
    #[case::partial_frame_header(&[0, 0], true)]
    #[case::partial_header_frame(&[&frame_header(4, H2_HEADERS, H2_END_HEADERS)[..], &[0]].concat(), true)]
    #[case::complete_block(&[&frame_header(1, H2_HEADERS, H2_END_HEADERS)[..], &[0]].concat(), false)]
    #[case::awaiting_continuation(&[&frame_header(1, H2_HEADERS, 0)[..], &[0]].concat(), true)]
    #[case::continued_block(
        &[
            &frame_header(1, H2_HEADERS, 0)[..],
            &[0],
            &frame_header(1, H2_CONTINUATION, H2_END_HEADERS),
            &[0],
        ].concat(),
        false,
    )]
    #[case::other_frames(&[&frame_header(1, SETTINGS, 0)[..], &[0]].concat(), false)]
    #[case::partial_data(&[&frame_header(4, DATA, 0)[..], &[0]].concat(), false)]
    fn header_blocks_are_open_until_end_headers(#[case] bytes: &[u8], #[case] open: bool) {
        let mut whole = HeaderBlocks::default();
        let mut split = HeaderBlocks::default();

        whole.feed(bytes);
        bytes.chunks(1).for_each(|byte| split.feed(byte));

        assert_eq!((whole.is_open(), split.is_open()), (open, open));
    }

    #[test]
    fn a_large_frame_length_uses_all_three_bytes() {
        let mut blocks = HeaderBlocks::default();

        blocks.feed(&[1, 2, 3, DATA, 0, 0, 0, 0, 1]);

        assert_eq!(blocks.payload_remaining, 0x01_02_03);
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
