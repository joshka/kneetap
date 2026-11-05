# ADR 0002: Crate boundaries

## Status

Accepted

## Context

What people install is one command, but it combines a server, a client, and process concerns such
as signals and privileges, which a library shouldn't touch.

## Decision

- `kneetap` is the installed command. It owns arguments, logging setup, signals, and privileges.
- `kneetap-server` is embeddable and holds no process-wide state.
- `kneetap-client` wraps `reqwest` so that its types stay out of the API.
- `kneetap-api` holds the report types and proto, so a client needn't depend on the server.

## Consequences

- Each boundary costs an error type, a builder, and some conversion code.
- The binary doesn't get smaller: the image needs `healthcheck`, so it contains the client too.
- All four version together. The libraries promise no stability before 1.0.

Revisit this if the libraries find no users by 1.0.
