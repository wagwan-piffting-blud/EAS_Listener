# Stage 1: Builder
FROM rust:1-slim AS builder

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /usr/src/app

RUN apt-get update && apt-get install -y pkg-config ffmpeg libssl-dev build-essential libc6 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./

RUN mkdir src && echo "fn main(){}" > src/main.rs

COPY src ./src
COPY include ./include
COPY tests ./tests

# TARGETARCH/BUILDARCH are injected automatically by buildx. The test suite only
# runs on the native leg of the matrix: under QEMU emulation (arm64 on an amd64
# runner) it would roughly double an already slow build for no extra signal,
# since the exact same tests already ran natively on amd64.
ARG TARGETARCH
ARG BUILDARCH

RUN touch -c src/main.rs \
    && if [ "${TARGETARCH}" = "${BUILDARCH}" ]; then \
           cargo test --release --locked; \
       else \
           echo "Skipping cargo test (emulated build: build=${BUILDARCH} target=${TARGETARCH})"; \
       fi \
    && cargo build --release --locked

# ----------------------------------------------------------------------------------------- #

# Stage 2: Runner
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV XDG_RUNTIME_DIR=/run/user/1000

# VARIANT=full -> Piper + espeak-ng, plus Speechify Tom on every arch we build
#                 for (amd64, arm64, armv7; see the SPFY_ASSET_* args below).
# VARIANT=lite -> Piper + espeak-ng only, on every arch. DEPRECATED; the `-lite`
#                 tag is still published so existing pulls keep working. See README.
ARG VARIANT=full
ARG TARGETARCH

ENV EAS_IMAGE_VARIANT=${VARIANT}

# Speechify Tom is enabled per-architecture by the SPFY_ASSET_SHA256_* args
# below. A non-empty checksum means "upstream publishes a spfy build for this
# arch, install it"; an empty one means "skip it here, and let the entrypoint
# fall back to Piper at runtime". As of the 2026.07.22 release every Linux arch
# we build for has a native asset, so all three are filled in. The legacy 32-bit
# "x86" asset still exists but is deliberately unused: selecting it is what
# would drag i386 multiarch back into the image.
#
# These checksums pin exact bytes. If a release is ever re-cut under the same
# tag, they must be refreshed or the build fails closed at `sha256sum -c`.
ARG SPFY_VERSION=2026.07.22
ARG SPFY_ASSET_SLUG_AMD64="x86_64"
ARG SPFY_ASSET_SHA256_AMD64="534464bca27e553e0a06e5478afe4e85c878d21cf28e4637d28175bd9370f18a"
ARG SPFY_ASSET_SLUG_ARM64="arm64"
ARG SPFY_ASSET_SHA256_ARM64="4994131c0fa61c3edb3c1e60ad138b0e9dc03b1a728b0c5e020708110ca0d7a2"
ARG SPFY_ASSET_SLUG_ARM="armv7"
ARG SPFY_ASSET_SHA256_ARM="345a63ad6acefb05216455ed5b516128ecc38c3baa01c24f7652f8cf8faf26c8"

# Voice data is architecture-independent: the same blobs are used by every spfy
# build. Pulled from a pinned commit and checksummed like the tarball itself.
ARG SPFY_VOICE_COMMIT=29f0888479de76b84ddc65e232a4ac04bee2f0dd
ARG SPFY_VIN_SHA256="5487ad30bcd9a96ce3fd313f74343f75096ecb11dc82dbd48f3f8c8dd7840d2c"
ARG SPFY_VDB_SHA256="e35bf4f8dbe5f608f0d0441e5b07acadd95b2b32d4102e6747c8ab43bf35d660"
ARG SPFY_VCF_SHA256="f6948a9ff2654af200220808bbe3f8d1feca1c72175994dcff7b64aa4368101c"

ARG PIPER_VERSION=2023.11.14-2
ARG PIPER_VOICE=en_US-lessac-medium

# Port the bundled Icecast server listens on / is exposed for the 24/7 alert
# stream. Keep in sync with ICECAST_ALERT_PORT in config.json and .env.
ARG ICECAST_ALERT_PORT=8000

# libssl3t64, not libssl3: Debian's 64-bit time_t transition renamed the OpenSSL
# 3 runtime, and on trixie `libssl3` has no installation candidate on any of our
# architectures. It only resolved on amd64/arm64 by virtual-package indirection,
# and on armhf it does not resolve at all -- so name the real package directly.
RUN mkdir -p /run/user/1000 && chown 1000:1000 /run/user/1000 \
    && mkdir -p /var/lib/apt/lists/partial \
    && apt-get update && apt-get install -y --no-install-recommends \
       libssl3t64 ca-certificates bash nginx jq ffmpeg curl apprise espeak-ng \
       php-fpm php-cli php-sqlite3 icecast2 \
    && rm -rf /var/lib/apt/lists/* \
    && chsh -s /bin/bash \
    && mkdir -p /data /var/www/html /app /app/piper

# Piper TTS binary + voice model. Ships on every architecture and every variant,
# so there is always a working engine even where Speechify cannot be installed.
RUN set -eu; \
    case "${TARGETARCH}" in \
        amd64) PIPER_ARCH=x86_64 ;; \
        arm64) PIPER_ARCH=aarch64 ;; \
        arm)   PIPER_ARCH=armv7l ;; \
        *) echo "Unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fL --retry 5 --retry-delay 2 -o /tmp/piper.tar.gz \
        "https://github.com/rhasspy/piper/releases/download/${PIPER_VERSION}/piper_linux_${PIPER_ARCH}.tar.gz"; \
    tar xzf /tmp/piper.tar.gz -C /app/piper --strip-components=1; \
    rm /tmp/piper.tar.gz; \
    curl -fL --retry 5 --retry-delay 2 -o "/app/piper/${PIPER_VOICE}.onnx" \
        "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/lessac/medium/en_US-lessac-medium.onnx"; \
    curl -fL --retry 5 --retry-delay 2 -o "/app/piper/${PIPER_VOICE}.onnx.json" \
        "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json"; \
    ln -sf /app/piper/piper /usr/local/bin/piper

# Speechify Tom, for VARIANT=full on any arch that has a published spfy build.
# Skipped (not failed) everywhere else, so the same Dockerfile keeps producing a
# valid image on architectures spfy has not reached yet and for the lite tag.
# The i386 branch only fires if someone points an arch at the legacy 32-bit
# "x86" asset; the native x86_64 and arm64 builds need no foreign architecture.
RUN set -eu; \
    case "${TARGETARCH}" in \
        amd64) SPFY_SLUG="${SPFY_ASSET_SLUG_AMD64}"; SPFY_SHA256="${SPFY_ASSET_SHA256_AMD64}" ;; \
        arm64) SPFY_SLUG="${SPFY_ASSET_SLUG_ARM64}"; SPFY_SHA256="${SPFY_ASSET_SHA256_ARM64}" ;; \
        arm)   SPFY_SLUG="${SPFY_ASSET_SLUG_ARM}";   SPFY_SHA256="${SPFY_ASSET_SHA256_ARM}" ;; \
        *)     SPFY_SLUG=""; SPFY_SHA256="" ;; \
    esac; \
    if [ "${VARIANT}" != "full" ]; then \
        echo "Skipping Speechify Tom: VARIANT=${VARIANT}. Piper and espeak-ng remain available."; \
        exit 0; \
    fi; \
    if [ -z "${SPFY_SHA256}" ]; then \
        echo "Skipping Speechify Tom: no spfy build is published for ${TARGETARCH} yet. Piper and espeak-ng remain available."; \
        exit 0; \
    fi; \
    if [ "${SPFY_SLUG}" = "x86" ]; then \
        dpkg --add-architecture i386; \
        apt-get update; \
        apt-get install -y --no-install-recommends libc6:i386; \
        rm -rf /var/lib/apt/lists/*; \
    fi; \
    ASSET_DIR="spfy-linux-${SPFY_SLUG}-${SPFY_VERSION}"; \
    ASSET_NAME="${ASSET_DIR}.tar.gz"; \
    curl -fL --retry 5 --retry-delay 2 -o "/tmp/${ASSET_NAME}" \
        "https://github.com/wagwan-piffting-blud/Speechify/releases/download/${SPFY_VERSION}/${ASSET_NAME}"; \
    echo "${SPFY_SHA256}  /tmp/${ASSET_NAME}" | sha256sum -c -; \
    tar -xzf "/tmp/${ASSET_NAME}" -C /usr/local/bin --strip-components=2 "${ASSET_DIR}/bin/spfy_synth"; \
    rm -f "/tmp/${ASSET_NAME}"; \
    chmod +x /usr/local/bin/spfy_synth; \
    mkdir -p /app/voices/tom; \
    for blob in tom.vin tom8.vdb tom.vcf; do \
        curl -fL --retry 5 --retry-delay 2 -o "/app/voices/tom/${blob}" \
            "https://raw.githubusercontent.com/wagwan-piffting-blud/Speechify/${SPFY_VOICE_COMMIT}/en-US/tom/${blob}"; \
    done; \
    printf '%s  /app/voices/tom/tom.vin\n%s  /app/voices/tom/tom8.vdb\n%s  /app/voices/tom/tom.vcf\n' \
        "${SPFY_VIN_SHA256}" "${SPFY_VDB_SHA256}" "${SPFY_VCF_SHA256}" | sha256sum -c -; \
    chmod -R 755 /app/voices

RUN userdel icecast2 && useradd -m -s /bin/bash icecast2 && chown -R icecast2:icecast2 /etc/icecast2 /var/log/icecast2

COPY --from=builder /usr/src/app/target/release/eas_listener /usr/local/bin/eas_listener
COPY ./docker_entrypoint.sh /docker_entrypoint.sh
COPY ./nginx.conf /etc/nginx/sites-available/default
COPY ./web_server/ /var/www/html
COPY ./Cargo.toml /app/Cargo.toml

WORKDIR /app

RUN chmod +x /docker_entrypoint.sh && chmod -R 777 /data /var/www/html

HEALTHCHECK --interval=10s --timeout=10s --retries=3 --start-period=5s CMD curl --fail http://localhost:${MONITORING_BIND_PORT}/api/health || exit 1

EXPOSE 80
EXPOSE ${MONITORING_BIND_PORT}
EXPOSE ${ICECAST_ALERT_PORT}

ENTRYPOINT ["/docker_entrypoint.sh"]
