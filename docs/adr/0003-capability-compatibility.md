# ADR 0003: Capability compatibility, not output compatibility

## Status

Accepted

## Context

kneetap covers what [whoami](https://github.com/traefik/whoami) offers. Matching its output exactly
would mean reproducing Go-specific formatting, permissive parsing, and known bugs.

## Decision

Match capabilities, and follow web conventions for everything else. There's no test suite that
compares output with whoami, because deliberate differences would make one mostly noise.

## Consequences

- Moving from whoami needs small changes, listed in the README.
- Invalid parameters return `400` rather than being ignored.
