################ build ################
FROM rust:1.84-bookworm AS builder

# ALSA for the audio path; libsoapysdr-dev + clang for the optional `sdr`
# feature's soapysdr-sys bindgen pass (stdbool.h lives inside the gcc include
# dir, which we point bindgen at via BINDGEN_EXTRA_CLANG_ARGS).
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    libasound2-dev \
    libclang-dev \
    libsoapysdr-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build with the SDR feature on. Needs BINDGEN_EXTRA_CLANG_ARGS so bindgen
# can find stdbool.h (shipped with gcc, not clang, on bookworm).
ENV BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/12/include
RUN cargo build --release --locked -p urc200-server --features sdr

################ runtime ################
# trixie (not bookworm) because the SoapySDR SDRplay module bind-mounted from
# the host requires GLIBCXX_3.4.32+ (needs GCC 13's libstdc++). bookworm's
# libstdc++6 is from GCC 12 and tops out at GLIBCXX_3.4.30.
FROM debian:trixie-slim

# libsoapysdr0.8 covers both urc200-server and the SoapySDR module loader.
# SDRplay-specific libs (libsdrplay_api + the libsdrPlaySupport Soapy module)
# are bind-mounted from the host at runtime via docker-compose so we don't
# have to bake the vendor API into the image.
#
# libasound2t64 on trixie (the t64 time_t transition rename) — use an
# alternative name if apt complains about the old package.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2t64 \
    libsoapysdr0.8 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Teach the dynamic linker about /usr/local/lib where the bind-mounted
# libsdrplay_api.so lives.
RUN echo '/usr/local/lib' > /etc/ld.so.conf.d/local.conf && ldconfig

# Static assets served by the binary via ServeDir.
COPY --from=builder /app/crates/urc200-server/static /opt/urc200/static
COPY --from=builder /app/target/release/urc200-server /usr/local/bin/urc200-server

# Run as the host user's dialout+audio groups so /dev/ttyUSB0 and /dev/snd/* work
# when passed through. Compose handles group_add at runtime.
ENV URC_STATIC=/opt/urc200/static \
    URC_BIND=0.0.0.0:3000 \
    URC_PORT=/dev/ttyUSB0 \
    RUST_LOG=urc200_server=info,urc200_serial=info,radio_sdr=info \
    SOAPY_SDR_PLUGIN_PATH=/usr/local/lib/SoapySDR/modules0.8 \
    LD_LIBRARY_PATH=/usr/local/lib:/usr/lib/x86_64-linux-gnu

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/urc200-server"]
