# ADR 0001: Why kneetap exists

## Status

Accepted

## Context

Debugging a proxy, load balancer, or service mesh means asking what actually reached a backend and
which backend answered. Small servers already answer that, but each covers part of the problem:
some are HTTP only, some leave TLS details out, and few bound what a misbehaving client can hold.

## Decision

Build one small server that reports what arrived over HTTP/1.1, HTTP/2, TLS, and gRPC, and echoes
WebSocket messages, with safe defaults and bounded resources, delivered as a binary, an image, and a
library.

## Consequences

- It stays a diagnostic tool. Features that don't help someone see what reached a backend stay out.
- Reflecting requests is the point, so inputs are reported rather than rejected wherever possible.
- Showing the server's environment needs the operator's opt-in. Request headers are always reported.
