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
3. `Deadlines` wraps the socket, closing silent connections and blocked writes. It keeps the
   connection permit, so an upgraded WebSocket still counts.
4. `hyper-util` detects HTTP/1.1 or HTTP/2.
5. Each request takes a request permit or gets a 503. `TrackedBody` holds that permit until the
   response finishes or the client disconnects.
6. The connection closes when the client leaves, the server stops, or it has no requests for the
   idle timeout. Requests count rather than traffic, because pings say nothing about need. An
   open WebSocket has no idle timeout, and holds its permit until it closes.

## A request

gRPC paths go to `grpc.rs`, and everything else reaches one fallback in `routes.rs`, because unknown
paths and methods are valid input. A report combines the host, the connection, and the request.

## Reloading certificates

`reload.rs` holds the current certificates. A replacement is built and validated completely, then
swapped in for new connections. Existing connections keep the certificates they started with.

## Shutdown

1. Health turns unhealthy, over HTTP and gRPC.
2. Listeners stop accepting, and idle connections close.
3. WebSockets get a going-away close frame.
4. Requests in progress finish, sharing one deadline. Anything left then is dropped.
