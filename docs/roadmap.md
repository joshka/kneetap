# Roadmap

## Before a first release

- Run the release workflows from end to end.
- Check runtime behavior on Windows and on each published image architecture.

## After that

- Report the TLS session over gRPC as well as HTTP.

## Ideas

Roughly in order of value. Each should help someone see what reached a backend, or which instance
answered.

- A request ID and received time in each report, so a cached response shows as a repeated ID, plus
  the server's start time and version, to spot restarts during a deploy.
- The connection each request arrived on, its request count, and when it opened, to show whether a
  proxy reuses or multiplexes backend connections.
- What the client offered in its TLS ClientHello: SNI, ALPN protocols, cipher suites, and
  signature schemes.
- An opt-in list of recent requests, to show traffic nobody sent by hand, such as load balancer
  health checks.
- `kneetap diff`, which sends a known request and lists what changed on the way, and
  `kneetap spray`, which counts which instance answers many requests.
- An explanation of the chain that forwarding headers claim, flagging contradictions such as
  `X-Forwarded-Proto: https` on a plain connection.
- Request trailers in reports, and an upload endpoint that reports the size and checksum of a large
  body without keeping it.
- PROXY protocol on listeners that opt in, reported apart from the connected peer.
- Server-sent events with server timestamps, for measuring how proxies buffer streams.
- A raw TCP echo, for testing below HTTP.

Out of scope unless that test changes: injecting faults such as stalls and resets.
