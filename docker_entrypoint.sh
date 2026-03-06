#!/bin/bash
set -eu

export ALREADY_SET_UP=false

if [ $(grep -c 'env\[MONITORING_BIND_PORT\]' /etc/php/8.4/fpm/pool.d/www.conf) -gt 0 ]; then
    export ALREADY_SET_UP=true
fi

if [ ! ${ALREADY_SET_UP:-} = "true" ]; then
    printf '' > /data/speechify_server.log

    ORIGINAL_LOCAL_DEEPLINK_HOST="${LOCAL_DEEPLINK_HOST:-}"

    convert_config_to_env() {
        local config_file="$1"
        local env_file="$2"
        local prefix="${3:-}"

        jq -r 'to_entries | .[] | "'"${prefix}"'" + (.key | ascii_upcase) + "=" + (if (.value | type) == "string" then .value else (if (.value | type) == "array" then (.value | @json) else (.value | tostring) end) end)' "$config_file" >> "$env_file"
    }

    eval "$(convert_config_to_env /app/config.json /app/.env "")"

    sed -i '/^FILTERS=/d' /app/.env
    while IFS= read -r env_line; do
        [ -z "$env_line" ] && continue
        [ "${env_line#\#}" != "$env_line" ] && continue
        env_key="${env_line%%=*}"
        env_value="${env_line#*=}"
        export "$env_key=$env_value"
    done < /app/.env

    if [ -n "${ORIGINAL_LOCAL_DEEPLINK_HOST:-}" ]; then
        export LOCAL_DEEPLINK_HOST="${ORIGINAL_LOCAL_DEEPLINK_HOST}"
    fi

    if [ -n "${ICECAST_STREAM_URL_MAPPING:-}" ]; then
        export ICECAST_STREAM_URL_MAPPING=$(echo "$ICECAST_STREAM_URL_MAPPING" | jq -cr 'to_entries | map((.key | @json) + ":" + (.value | @json)) | "{" + join(",") + "}" | gsub("\\\\"; "\\\\\\\\") | gsub("\""; "\\\\\"") | gsub("\u0027"; "\u0027\\\\\u0027\u0027")')
    else
        export ICECAST_STREAM_URL_MAPPING="{}"
    fi

    su -www-data -s /bin/bash -c "echo 'env[MONITORING_BIND_PORT] = ${MONITORING_BIND_PORT:-8080}' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[USE_REVERSE_PROXY] = \"${USE_REVERSE_PROXY}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    if [ "${USE_REVERSE_PROXY:-false}" = "true" ]; then
        su -www-data -s /bin/bash -c "printf 'env[WS_REVERSE_PROXY_URL] = \"${WS_REVERSE_PROXY_URL}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
        su -www-data -s /bin/bash -c "printf 'env[REVERSE_PROXY_URL] = \"${REVERSE_PROXY_URL}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    fi
    su -www-data -s /bin/bash -c "printf 'env[DASHBOARD_USERNAME] = \"${DASHBOARD_USERNAME}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[DASHBOARD_PASSWORD] = \"${DASHBOARD_PASSWORD}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[SHARED_STATE_DIR] = \"${SHARED_STATE_DIR}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[RECORDING_DIR] = \"${RECORDING_DIR}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[DEDICATED_ALERT_LOG_FILE] = \"${DEDICATED_ALERT_LOG_FILE}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[MONITORING_MAX_LOGS] = \"${MONITORING_MAX_LOGS:-}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[ICECAST_STREAM_URL_MAPPING] = \"${ICECAST_STREAM_URL_MAPPING}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[WATCHED_FIPS] = \"${WATCHED_FIPS:-}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[TZ] = \"${TZ}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[ALERT_SOUND_SRC] = \"${ALERT_SOUND_SRC}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[ALERT_SOUND_ENABLED] = \"${ALERT_SOUND_ENABLED}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    sed -i "s/session.gc_maxlifetime = .*/session.gc_maxlifetime = 259200/" /etc/php/8.4/fpm/php.ini

    export ALREADY_SET_UP=true
fi

chmod -R 777 /app /data /var/www/html

if [ "${START_ICECAST:-false}" = "true" ]; then
    ICECAST_CONFIG="${ICECAST_CONFIG_PATH:-/etc/icecast2/icecast.xml}"
    if [ ! -f "$ICECAST_CONFIG" ]; then
        echo "Icecast config not found at $ICECAST_CONFIG" >&2
        exit 1
    fi

    echo "Starting Icecast..."
    if ! su -s /bin/bash -c "icecast2 -c \"$ICECAST_CONFIG\" -b" icecast2; then
        echo "Failed to start Icecast with $ICECAST_CONFIG" >&2
        exit 1
    fi
fi

if [ "${PROCESS_CAP_ALERTS:-true}" = "false" ]; then
    export PROCESS_CAP_ALERTS=false
else
    export PROCESS_CAP_ALERTS=true

    export WINEDLLOVERRIDES="winemenubuilder.exe=d"
    export WINEDEBUG=-all

    tmux new-session -d -s speechify_server 'xvfb-run -a --server-args="-screen 0 1024x768x24 -nolisten tcp" /usr/lib/wine/wine /app/Speechify/bin/Speechify.exe >> /data/speechify_server.log 2>&1'

    echo "Waiting for Speechify server to start..."

    SERVER_LOG_FILE="/data/speechify_server.log"
    SERVER_READY_MESSAGE="Server started, waiting for connections"
    SERVER_STARTUP_TIMEOUT_SECONDS=120
    SERVER_STARTUP_DEADLINE=$((SECONDS + SERVER_STARTUP_TIMEOUT_SECONDS))

    until [ "$SECONDS" -ge "$SERVER_STARTUP_DEADLINE" ]; do
        if [ -f "$SERVER_LOG_FILE" ] && grep -Fq "$SERVER_READY_MESSAGE" "$SERVER_LOG_FILE"; then
            echo "Speechify server is ready."
            break
        fi
        sleep 1
    done

    if ! [ -f "$SERVER_LOG_FILE" ] || ! grep -Fq "$SERVER_READY_MESSAGE" "$SERVER_LOG_FILE"; then
        echo "Timed out waiting for Speechify server startup message in $SERVER_LOG_FILE" >&2
        [ -f "$SERVER_LOG_FILE" ] && tail -n 50 "$SERVER_LOG_FILE" >&2
        exit 1
    fi
fi

php-fpm8.4 -R
nginx
eas_listener
