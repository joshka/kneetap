# Roadmap

## Before a first release

- Run the release workflows from end to end.
- Check runtime behavior on Windows and on each published image architecture.

## After that

- Publish the crates to crates.io.
- Report the TLS session over gRPC as well as HTTP.

## Ideas

- A raw TCP echo, for testing below HTTP.
- Server-sent events, for testing how proxies buffer streams.
