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

RUN touch -c src/main.rs && cargo test --release --locked && cargo build --release --locked

# ----------------------------------------------------------------------------------------- #

# Stage 2: Runner
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV XDG_RUNTIME_DIR=/run/user/1000
ENV TTS_ENGINE=speechify

ARG VERSION=2026.07.21
ARG ASSET_NAME="spfy-linux-x86-${VERSION}.tar.gz"
ARG ASSET_SHA256="e9c2fe8557f4d80f56f4740babbf914c1b492bb5c209eff9f1a1d99ded1f1ad1"
ARG ICECAST_ALERT_PORT=8000

RUN mkdir -p /run/user/1000 && chown 1000:1000 /run/user/1000 && mkdir -p /var/lib/apt/lists/partial && dpkg --add-architecture i386 && apt-get update && apt-get install -y --no-install-recommends libssl3 libc6:i386 ca-certificates git bash nginx jq ffmpeg curl apprise php-fpm php-cli php-sqlite3 icecast2 && rm -rf /var/lib/apt/lists/* && chsh -s /bin/bash && mkdir -p /data /var/www/html /app

RUN userdel icecast2 && useradd -m -s /bin/bash icecast2 && chown -R icecast2:icecast2 /etc/icecast2 /var/log/icecast2

COPY --from=builder /usr/src/app/target/release/eas_listener /usr/local/bin/eas_listener
COPY ./docker_entrypoint.sh /docker_entrypoint.sh
COPY ./nginx.conf /etc/nginx/sites-available/default
COPY ./web_server/ /var/www/html
COPY ./Cargo.toml /app/Cargo.toml

WORKDIR /app

RUN curl -fL --retry 5 --retry-delay 2 -o "/tmp/${ASSET_NAME}" "https://github.com/wagwan-piffting-blud/Speechify/releases/download/${VERSION}/${ASSET_NAME}" \
    && echo "${ASSET_SHA256}  /tmp/${ASSET_NAME}" | sha256sum -c - \
    && tar -xzf "/tmp/${ASSET_NAME}" -C /usr/local/bin --strip-components=2 "spfy-linux-x86-${VERSION}/bin/spfy_synth" \
    && rm -f "/tmp/${ASSET_NAME}" \
    && chmod +x /usr/local/bin/spfy_synth /docker_entrypoint.sh \
    && chmod -R 777 /data /var/www/html

RUN git clone --no-checkout --depth 1 --filter=blob:none https://github.com/wagwan-piffting-blud/Speechify.git /tmp/voices \
    && git -C /tmp/voices sparse-checkout set --no-cone en-US/tom \
    && git -C /tmp/voices checkout \
    && mkdir -p /app/voices/tom \
    && cp /tmp/voices/en-US/tom/tom.vin /tmp/voices/en-US/tom/tom8.vdb /tmp/voices/en-US/tom/tom.vcf /app/voices/tom/ \
    && rm -rf /tmp/voices \
    && chmod -R 755 /app/voices

HEALTHCHECK --interval=10s --timeout=10s --retries=3 --start-period=5s CMD curl --fail http://localhost:${MONITORING_BIND_PORT}/api/health || exit 1

EXPOSE 80
EXPOSE ${MONITORING_BIND_PORT}
EXPOSE ${ICECAST_ALERT_PORT}

ENTRYPOINT ["/docker_entrypoint.sh"]
