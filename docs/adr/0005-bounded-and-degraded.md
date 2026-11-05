# ADR 0005: Bounded resources and degraded startup

## Status

Accepted

## Context

kneetap is deployed where something is already wrong, and it faces clients that misbehave. It
should still start, and one client shouldn't be able to stop it serving others.

## Decision

Everything a client can hold has a limit. Startup continues with whatever works and reports what it
worked around. `--strict` turns those problems into failures.

Security is the exception. A broken client authority stops HTTPS rather than weakening it, and a
failed privilege drop stops the process.

## Consequences

- A running server may not be the one you configured, so reports show degradations.
- Defaults have to suit both a laptop and a load test.
