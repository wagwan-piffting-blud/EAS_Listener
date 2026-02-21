# Stage 1: Builder
FROM rust:1-slim AS builder

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /usr/src/app

RUN apt-get update && apt-get install -y pkg-config ffmpeg libssl-dev build-essential libc6 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./

RUN mkdir src && echo "fn main(){}" > src/main.rs
RUN cargo build --release --locked

COPY src ./src

RUN touch -c src/main.rs
RUN cargo build --release --locked

# ----------------------------------------------------------------------------------------- #

# Stage 2: Runner
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    python3 \
    python3-pip \
    git \
    bash \
    nginx \
    jq \
    ffmpeg \
    curl \
    php-fpm php-cli php-mysql php-curl php-gd php-mbstring php-xml php-zip \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY ./requirements.txt /requirements.txt

RUN pip3 install -r /requirements.txt --break-system-packages

COPY decoder.py /usr/local/bin/decoder.py

RUN chmod +x /usr/local/bin/decoder.py
RUN mkdir -p /data /var/www/html

WORKDIR /data

COPY --from=builder /usr/src/app/target/release/eas_listener /usr/local/bin/eas_listener
COPY ./docker_entrypoint.sh /docker_entrypoint.sh
COPY ./nginx.conf /etc/nginx/sites-available/default
COPY ./web_server/ /var/www/html
COPY ./Cargo.toml /app/Cargo.toml

RUN chmod +x /docker_entrypoint.sh && chmod -R 777 /var/www/html

HEALTHCHECK --interval=10s --timeout=10s --retries=3 --start-period=5s CMD curl --fail http://localhost:${MONITORING_BIND_PORT}/api/health || exit 1

EXPOSE 80
EXPOSE ${MONITORING_BIND_PORT}

ENTRYPOINT ["/docker_entrypoint.sh"]
