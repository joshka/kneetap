# ADR 0006: Separate HTTP and HTTPS listeners

## Status

Accepted

## Context

A server can serve one protocol per port or detect TLS on a shared port. Containers usually expect
fixed ports for each, and detection adds a code path that exists only for convenience.

## Decision

Serve HTTP and HTTPS on separate ports, both on by default.

## Consequences

- `--no-http` and `--no-tls` choose one protocol.
- `--cacert` only protects the HTTPS port, so the server warns if HTTP is also on.
