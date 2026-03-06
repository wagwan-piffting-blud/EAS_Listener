# Stage 1: Builder
FROM rust:1-slim AS builder

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /usr/src/app

RUN apt-get update && apt-get install -y pkg-config ffmpeg libssl-dev build-essential libc6 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./

RUN mkdir src && echo "fn main(){}" > src/main.rs
RUN cargo build --release --locked

COPY src ./src
COPY include ./include

RUN touch -c src/main.rs
RUN cargo build --release --locked

# ----------------------------------------------------------------------------------------- #

# Stage 2: Runner
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV XDG_RUNTIME_DIR=/run/user/1000

ARG VERSION=1
ARG ASSET_NAME=Speechify.7z
ARG ASSET_SHA256="b1df25c9ef0322d0b419e7bfcb4d563ff62569c6fadcc546128efb9981eb6a4c"

RUN mkdir -p /run/user/1000 && chown 1000:1000 /run/user/1000 && mkdir -p /var/lib/apt/lists/partial && apt-get update && apt-get install -y --no-install-recommends wget libssl3 ca-certificates gnupg2 xvfb xauth git bash nginx jq ffmpeg curl bash p7zip-full tmux php-fpm php-cli php-mysql php-curl php-gd php-mbstring php-xml php-zip icecast2 liquidsoap && mkdir -pm755 /etc/apt/keyrings && wget -O - https://dl.winehq.org/wine-builds/winehq.key | gpg --dearmor -o /etc/apt/keyrings/winehq-archive.key - && dpkg --add-architecture i386 && wget -NP /etc/apt/sources.list.d/ https://dl.winehq.org/wine-builds/debian/dists/trixie/winehq-trixie.sources && apt-get update && apt-get install -y --no-install-recommends winehq-stable wine64 wine32 && rm -rf /var/lib/apt/lists/* && chsh -s /bin/bash && mkdir -p /data /var/www/html /app

RUN userdel icecast2 && useradd -m -s /bin/bash icecast2 && chown -R icecast2:icecast2 /etc/icecast2 /var/log/icecast2

COPY --from=builder /usr/src/app/target/release/eas_listener /usr/local/bin/eas_listener
COPY ./docker_entrypoint.sh /docker_entrypoint.sh
COPY ./nginx.conf /etc/nginx/sites-available/default
COPY ./web_server/ /var/www/html
COPY ./Cargo.toml /app/Cargo.toml

WORKDIR /app

RUN curl -fL --retry 5 --retry-delay 2 -o "/tmp/${ASSET_NAME}" "https://github.com/wagwan-piffting-blud/Speechify_EAS_Listener/releases/download/v${VERSION}/${ASSET_NAME}" && echo "${ASSET_SHA256}  /tmp/${ASSET_NAME}" | sha256sum -c - && 7z x "/tmp/${ASSET_NAME}" -o/app/Speechify && rm -f "/tmp/${ASSET_NAME}" && chmod +x /docker_entrypoint.sh && chmod -R 777 /data /var/www/html

HEALTHCHECK --interval=10s --timeout=10s --retries=3 --start-period=5s CMD curl --fail http://localhost:${MONITORING_BIND_PORT}/api/health || exit 1

EXPOSE 80
EXPOSE ${MONITORING_BIND_PORT}
EXPOSE 8000

ENTRYPOINT ["/docker_entrypoint.sh"]
