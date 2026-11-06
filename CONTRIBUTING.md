# Contributing

Install stable and nightly Rust, [just](https://just.systems), and the tools listed in the
[justfile](justfile). Nightly is only used for formatting.

```sh
just fmt   # format the code
just ci    # run every check that CI runs
```

Project conventions are in [docs/conventions](docs/conventions).

## Releasing

Releases start by hand:

1. Run the Release PR workflow, which prepares the next version and changelog.
2. Review the PR. It's opened with the workflow's own token, so GitHub won't start CI for it by
   itself: close and reopen it, or approve its runs, then merge it and tag the merged commit as
   `v` plus the version, such as `v0.1.1`. `release-plz` finds the last release from that tag.
3. Run the Release workflow on that tag, which publishes binaries and images.

The crates aren't published to crates.io yet.
