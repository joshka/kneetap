# Requires cargo-deny, cargo-nextest, rumdl, and typos.

default:
    @just --list

# Format the code. Formatting uses unstable rustfmt options.
fmt:
    cargo +nightly fmt --all

fmt-check:
    cargo +nightly fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
    cargo nextest run --workspace --all-features --locked --no-tests=pass

docs:
    RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --document-private-items --locked

deny:
    cargo deny --all-features --config .config/deny.toml check

typos:
    typos

lint-md:
    rumdl check .

# Run every check that CI runs.
ci: fmt-check clippy test docs deny typos lint-md
