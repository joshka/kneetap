use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use axum::extract::{FromRequestParts, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::http::uri::Authority;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::limits::Activity;
use crate::routes::{Context, Rejection};

/// Upgrades a request to a WebSocket that echoes messages.
pub async fn upgrade(context: &Context, parts: &mut Parts) -> Result<Response, Rejection> {
    if !context.allow_any_ws_origin {
        check_origin(parts)?;
    }
    let upgrade = WebSocketUpgrade::from_request_parts(parts, &()).await;
    let upgrade = upgrade.map_err(|_| (StatusCode::BAD_REQUEST, "WebSocket upgrade required\n"))?;
    let limit = context.max_ws_message_size;
    // An HTTP/2 WebSocket shares its connection, which must not count as idle while this is open.
    let guard = parts.extensions.get::<Arc<Activity>>().map(Activity::begin);
    // Taken before the upgrade completes, so that shutdown also waits for a pending upgrade.
    let tracked = context.websockets.token();
    let stop = context.stop.clone();
    let close_timeout = context.shutdown_timeout;
    Ok(upgrade
        .max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| async move {
            let _held = (guard, tracked);
            echo(socket, stop, close_timeout).await;
        })
        .into_response())
}

/// Refuses browser pages from other sites, which would otherwise reach this server through a
/// visitor's network. Clients that send no `Origin` aren't browsers, so the check doesn't apply.
fn check_origin(parts: &Parts) -> Result<(), Rejection> {
    let Some(origin) = parts.headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let authority = request_authority(parts).unwrap_or_default();
    let same = origin
        .to_str()
        .ok()
        .and_then(|origin| same_origin(origin, authority));
    let same = same.ok_or((StatusCode::BAD_REQUEST, "invalid origin\n"))?;
    if same {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "cross-origin WebSocket denied\n"))
    }
}

/// Returns the authority the client asked for, which HTTP/2 sends in the URI and HTTP/1 in `Host`.
fn request_authority(parts: &Parts) -> Option<&str> {
    if let Some(authority) = parts.uri.authority() {
        return Some(authority.as_str());
    }
    parts.headers.get(header::HOST)?.to_str().ok()
}

/// Compares a browser `Origin` with the request's authority.
///
/// The scheme is ignored because a TLS-terminating proxy makes `https` origins reach this server
/// over plain HTTP. An authority without a port only matches an origin on its default port.
fn same_origin(origin: &str, authority: &str) -> Option<bool> {
    let origin = Url::parse(origin).ok()?;
    let authority = authority.parse::<Authority>().ok()?;
    let same_port = match authority.port_u16() {
        Some(port) => origin.port_or_known_default() == Some(port),
        None => origin.port().is_none(),
    };
    Some(same_port && origin.host_str()?.eq_ignore_ascii_case(authority.host()))
}

/// Echoes text and binary messages until either side closes, or the server shuts down.
async fn echo(mut socket: WebSocket, stop: CancellationToken, close_timeout: Duration) {
    loop {
        let received = tokio::select! {
            received = socket.recv() => received,
            () = stop.cancelled() => return go_away(socket, close_timeout).await,
        };
        match received {
            Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                if socket.send(message).await.is_err() {
                    return;
                }
            }
            // The next `recv` flushes the queued close reply, then ends the stream.
            Some(Ok(Message::Close(_) | Message::Ping(_) | Message::Pong(_))) => {}
            Some(Err(_)) | None => return,
        }
    }
}

/// Tells the client the server is shutting down, then waits up to `timeout` for its close reply.
///
/// A client that can't be told is already gone.
async fn go_away(mut socket: WebSocket, timeout: Duration) {
    let close = CloseFrame {
        code: close_code::AWAY,
        reason: "server shutting down".into(),
    };
    if socket.send(Message::Close(Some(close))).await.is_err() {
        return;
    }
    let replied = async { while let Some(Ok(_)) = socket.recv().await {} };
    let _ = tokio::time::timeout(timeout, replied).await;
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("https://app.test", "app.test", Some(true))]
    #[case("http://app.test", "app.test", Some(true))]
    #[case("https://APP.test", "app.test", Some(true))]
    #[case("https://app.test:8443", "app.test:8443", Some(true))]
    #[case("https://app.test", "app.test:443", Some(true))]
    #[case("https://app.test:8443", "app.test", Some(false))]
    #[case("https://evil.test", "app.test", Some(false))]
    #[case("not a url", "app.test", None)]
    fn origins_match_on_host_and_port_across_schemes(
        #[case] origin: &str,
        #[case] authority: &str,
        #[case] expected: Option<bool>,
    ) {
        assert_eq!(same_origin(origin, authority), expected);
    }
}
