# Robustness conventions

- Treat anything from the network as untrusted. Limit how much is read before buffering it, and
  escape control characters before printing it to a terminal.
- A long-running process handles both `SIGINT` and `SIGTERM`, and a real-process test proves that
  work in progress drains.
- Protocol edge cases follow the RFC. Name the section in a comment when the behavior isn't obvious,
  and add a test for the case.
