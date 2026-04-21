################ build ################
FROM rust:1.84-bookworm AS builder

# ALSA headers for cpal (audio comes in a later epic; the server itself doesn't
# link libasound today, but the dep tree on `alsa-sys` compiles regardless and
# this future-proofs Epic 4).
RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked -p urc200-server

################ runtime ################
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Static assets served by the binary via ServeDir.
COPY --from=builder /app/crates/urc200-server/static /opt/urc200/static
COPY --from=builder /app/target/release/urc200-server /usr/local/bin/urc200-server

# Run as the host user's dialout+audio groups so /dev/ttyUSB0 and /dev/snd/* work
# when passed through. Compose handles group_add at runtime.
ENV URC_STATIC=/opt/urc200/static \
    URC_BIND=0.0.0.0:3000 \
    URC_PORT=/dev/ttyUSB0 \
    RUST_LOG=urc200_server=info,urc200_serial=info

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/urc200-server"]
