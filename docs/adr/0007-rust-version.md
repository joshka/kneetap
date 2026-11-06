# ADR 0007: Rust version policy

## Status

Accepted

## Context

Libraries often support Rust versions well behind stable, for the sake of their users.

## Decision

`rust-version` tracks current stable, because kneetap ships mainly as an application and users
install binaries or images. A daily job on beta finds problems before they reach stable.

## Consequences

The libraries need a recent compiler. Revisit this at first publish, or if library users ask for
older versions.
