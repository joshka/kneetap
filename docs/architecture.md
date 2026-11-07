# Architecture

kneetap is a workspace of four crates. [ADR 0002](adr/0002-crate-boundaries.md) explains the split.

```text
kneetap ──────────┬──> kneetap-server ──┐
(the command)     │                     ├──> kneetap-api
                  └──> kneetap-client ──┘
```

## Startup

Order matters, because some steps need privileges that later steps give up.

1. `kneetap` binds its listeners, moving to fallback ports if it has to.
2. `ServerBuilder::build` reads certificates.
3. `kneetap` drops privileges.
4. `kneetap` starts the runtime and calls `Server::serve`.

## A connection

`transport.rs` owns each connection from accept to close:

1. A connection permit is taken, or the connection is closed.
2. TLS listeners complete the handshake within its deadline.
3. `Deadlines` wraps the socket, closing silent connections, incomplete HTTP/2 header blocks, and
   blocked writes. It keeps the connection permit, so an upgraded WebSocket still counts.
4. `hyper-util` detects HTTP/1.1 or HTTP/2.
5. Each request takes a request permit or gets a 503 with no body and its reason in
   `x-kneetap-refusal`. `TrackedBody` holds that permit until the response finishes or the client
   disconnects.
6. The connection closes when the client leaves, the server stops, or it has no requests for the
   idle timeout. Requests count rather than traffic, because pings say nothing about need. A
   WebSocket or gRPC stream with no messages for that long ends too. An HTTP/2 response stalled by
   flow control for the write timeout closes the connection, because hyper stops polling its body
   and can't reset one stream from outside.

## A request

gRPC paths go to `grpc.rs`, and everything else reaches one fallback in `routes.rs`, because unknown
paths and methods are valid input. A report combines the host, the connection, and the request.

## Reloading certificates

`reload.rs` holds the current certificates. A replacement is built and validated completely, then
swapped in for new connections. Existing connections keep the certificates they started with.

## Shutdown

1. Health turns unhealthy, over HTTP and gRPC.
2. Listeners stop accepting, and idle connections close.
3. WebSockets get a going-away close frame, and quiet gRPC streams end as `UNAVAILABLE`.
4. Requests in progress finish, sharing one deadline. Anything left then is dropped.
