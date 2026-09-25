# Stage 1: Builder
FROM --platform=$BUILDPLATFORM rust:1-slim AS builder

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_INCREMENTAL=0
ENV CARGO_NET_RETRY=5
ENV CARGO_TERM_COLOR=never
WORKDIR /usr/src/app
ARG TARGETARCH
ARG BUILDARCH

RUN set -eu; \
    case "${TARGETARCH}" in \
        amd64) DEB_ARCH=amd64; CROSS_PKG="gcc-x86-64-linux-gnu" ;; \
        arm64) DEB_ARCH=arm64; CROSS_PKG="gcc-aarch64-linux-gnu" ;; \
        arm)   DEB_ARCH=armhf; CROSS_PKG="gcc-arm-linux-gnueabihf" ;; \
        *) echo "Unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    if [ "${TARGETARCH}" = "${BUILDARCH}" ]; then CROSS_PKG=""; fi; \
    if [ "${DEB_ARCH}" != "$(dpkg --print-architecture)" ]; then \
        dpkg --add-architecture "${DEB_ARCH}"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        pkg-config build-essential ${CROSS_PKG} \
        "libssl-dev:${DEB_ARCH}" "libc6-dev:${DEB_ARCH}"; \
    rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "${TARGETARCH}" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-gnu;      GNU_TRIPLE=x86_64-linux-gnu ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-gnu;     GNU_TRIPLE=aarch64-linux-gnu ;; \
        arm)   RUST_TARGET=armv7-unknown-linux-gnueabihf; GNU_TRIPLE=arm-linux-gnueabihf ;; \
        *) echo "Unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    { \
        echo "export RUST_TARGET=${RUST_TARGET}"; \
        if [ "${TARGETARCH}" != "${BUILDARCH}" ]; then \
            echo "export CARGO_TARGET_$(echo "${RUST_TARGET}" | tr 'a-z-' 'A-Z_')_LINKER=${GNU_TRIPLE}-gcc"; \
            echo "export CC_$(echo "${RUST_TARGET}" | tr '-' '_')=${GNU_TRIPLE}-gcc"; \
            echo "export PKG_CONFIG_ALLOW_CROSS=1"; \
            echo "export PKG_CONFIG_SYSROOT_DIR=/"; \
            echo "export PKG_CONFIG_PATH=/usr/lib/${GNU_TRIPLE}/pkgconfig"; \
        fi; \
    } > /etc/cross.env; \
    rustup target add "${RUST_TARGET}"

COPY Cargo.toml Cargo.lock ./
RUN set -eu; \
    . /etc/cross.env; \
    mkdir -p src; \
    echo 'fn main() {}' > src/main.rs; \
    cargo build --release --locked --target "${RUST_TARGET}"; \
    rm -rf src \
       "target/${RUST_TARGET}/release/eas_listener" \
       "target/${RUST_TARGET}/release/deps/eas_listener"*

COPY include ./include
COPY src ./src
COPY build.rs ./
# Baked into the binary by build.rs. The image serves /app/web_server as well, which wins while it
# is there; the built-in copy is what a bind mount or a stripped image falls back to.
COPY web_server ./web_server
# Compiled in as the built-in pronunciation dictionary.
COPY cap_tts_replacement_config.example.json ./

RUN set -eu; \
    . /etc/cross.env; \
    find src include web_server build.rs cap_tts_replacement_config.example.json -type f -exec touch {} +; \
    cargo build --release --locked --target "${RUST_TARGET}"; \
    cp "target/${RUST_TARGET}/release/eas_listener" /usr/local/bin/eas_listener

# ----------------------------------------------------------------------------------------- #

# Stage 2: Runner
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV XDG_RUNTIME_DIR=/run/user/1000
# The binary installs to /usr/local/bin but its assets live in /app, so the root cannot be
# inferred from the executable's location the way a portable install allows.
ENV EAS_APP_ROOT=/app
ARG VARIANT=full
ARG TARGETARCH
ENV EAS_IMAGE_VARIANT=${VARIANT}
ARG PIPER_VERSION=2023.11.14-2
ARG PIPER_VOICE=en_US-lessac-medium
ARG ICECAST_ALERT_PORT=8000

RUN set -eu; \
    printf 'path-exclude /usr/share/man/*\npath-exclude /usr/share/doc/*\npath-include /usr/share/doc/*/copyright\n' \
        > /etc/dpkg/dpkg.cfg.d/01-nodoc; \
    printf 'Acquire::Languages "none";\n' > /etc/apt/apt.conf.d/01-no-languages; \
    mkdir -p /run/user/1000; \
    chown 1000:1000 /run/user/1000; \
    mkdir -p /var/lib/apt/lists/partial; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        libssl3t64 ca-certificates bash jq ffmpeg curl espeak-ng \
        7zip; \
    rm -rf /var/lib/apt/lists/*; \
    chsh -s /bin/bash; \
    mkdir -p /data /app/web_server /app /app/piper

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

# No TTS engine is in the image. The entrypoint fetches the configured one -- cep6, loqdave or
# Speechify, each from its own pinned release -- onto the state volume at container start, so
# none of them is part of any published image.
COPY tools/components.json tools/fetch_components.sh /app/tools/
COPY tts_voices/cep6/fetch_voices.sh /app/tts_voices/cep6/fetch_voices.sh

COPY --from=builder /usr/local/bin/eas_listener /usr/local/bin/eas_listener
COPY ./docker_entrypoint.sh /docker_entrypoint.sh
COPY ./web_server/ /app/web_server
COPY ./Cargo.toml /app/Cargo.toml

WORKDIR /app

RUN chmod +x /docker_entrypoint.sh /app/tools/fetch_components.sh /app/tts_voices/cep6/fetch_voices.sh \
    && chmod -R 777 /data /app/web_server \
    && /app/tools/fetch_components.sh apprise

# The port comes from config.json, which this check cannot read: it does not run inside the
# entrypoint's environment. Whichever server bound the port -- the listener or first-run setup --
# writes where it can be reached to this file, and a missing file means nothing is listening yet.
# The start period covers a first boot, which downloads the chosen TTS engine before binding.
ENV EAS_HEALTH_ADDR_FILE=/tmp/eas_listener.addr
HEALTHCHECK --interval=10s --timeout=10s --retries=3 --start-period=120s \
    CMD curl -fsS -o /dev/null "http://$(cat "$EAS_HEALTH_ADDR_FILE" 2>/dev/null)/api/health" || exit 1

# The dashboard, and the alert stream the listener serves itself on ICECAST_ALERT_PORT. The
# defaults; publish whatever MONITORING_BIND_PORT and ICECAST_ALERT_PORT are set to.
EXPOSE 8080
EXPOSE ${ICECAST_ALERT_PORT}

ENTRYPOINT ["/docker_entrypoint.sh"]
