# Cross-build on the build host so ARMv7 never needs an unsupported Rust compiler host image.
FROM --platform=$BUILDPLATFORM rust:1.99-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends python3-pip \
    && pip3 install --break-system-packages ziglang==0.17.0 \
    && cargo install cargo-zigbuild --version 0.24.1 --locked \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG TARGETARCH
ARG TARGETVARIANT
RUN case "$TARGETARCH/$TARGETVARIANT" in \
      amd64/*) target=x86_64-unknown-linux-musl ;; \
      arm64/*) target=aarch64-unknown-linux-musl ;; \
      arm/v7) target=armv7-unknown-linux-musleabihf ;; \
      *) echo "unsupported target: $TARGETARCH/$TARGETVARIANT" >&2; exit 1 ;; \
    esac \
    && rustup target add "$target" \
    && cargo zigbuild --release --locked --target "$target" --package kneetap --bin kneetap \
    && mkdir /out && cp "target/$target/release/kneetap" /out/kneetap

FROM scratch
COPY --from=build /out/kneetap /kneetap
COPY LICENSE-APACHE LICENSE-MIT /
EXPOSE 80 443
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD ["/kneetap", "healthcheck"]
ENTRYPOINT ["/kneetap", "serve"]
