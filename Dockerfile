# syntax=docker/dockerfile:1.7
FROM rust:1.82-bookworm AS builder
WORKDIR /src
COPY Cargo.toml rust-toolchain.toml ./
COPY src ./src
ARG AEGIS_FEATURES=parquet-data
RUN cargo build --release --features "$AEGIS_FEATURES"

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/project-aegis /usr/local/bin/project-aegis
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/project-aegis"]
CMD ["shadow", "BTCUSDT"]
