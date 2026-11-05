# ADR 0004: A custom connection loop

## Status

Accepted

## Context

`axum::serve` can't pass handshake details to handlers, and doesn't own enough of a connection's
lifetime to enforce limits or a shutdown deadline.

## Decision

Accept connections directly and serve each one with `hyper-util`, keeping handlers independent of
listeners. The loop stays specific to this server rather than becoming a framework.

## Consequences

- Timeouts that HTTP can't see, such as a silent connection, are handled by wrapping the socket.
- This code needs real-socket tests, because its failures don't appear in handler tests.
