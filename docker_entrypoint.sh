#!/bin/bash
set -eu

printf '' > /data/speechify_server.log

ORIGINAL_LOCAL_DEEPLINK_HOST="${LOCAL_DEEPLINK_HOST:-}"

convert_config_to_env() {
    local config_file="$1"
    local env_file="$2"
    local prefix="${3:-}"

    jq -r 'to_entries | .[] | "'"${prefix}"'" + (.key | ascii_upcase) + "=" + (if (.value | type) == "string" then .value else (if (.value | type) == "array" then (.value | @json) else (.value | tostring) end) end)' "$config_file" >> "$env_file"
}

: > /app/.env
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

sed -i "s/session.gc_maxlifetime = .*/session.gc_maxlifetime = 259200/" /etc/php/8.4/fpm/php.ini

chmod -R 777 /app /data /var/www/html

if [ "${START_ICECAST:-false}" = "true" ] || [ "${ICECAST_ALERT_STREAM_ENABLED:-false}" = "true" ]; then
    ICECAST_CONFIG="${ICECAST_CONFIG_PATH:-/etc/icecast2/icecast.xml}"
    if [ ! -f "$ICECAST_CONFIG" ]; then
        echo "Icecast config not found at $ICECAST_CONFIG" >&2
        exit 1
    fi

    ICECAST_LISTEN_PORT="${ICECAST_ALERT_PORT:-8000}"
    ICECAST_RUNTIME_CONFIG="/app/icecast.runtime.xml"
    sed '0,/<port>[0-9]*<\/port>/s//<port>'"$ICECAST_LISTEN_PORT"'<\/port>/' "$ICECAST_CONFIG" > "$ICECAST_RUNTIME_CONFIG"
    chmod 644 "$ICECAST_RUNTIME_CONFIG"
    ICECAST_CONFIG="$ICECAST_RUNTIME_CONFIG"

    echo "Starting Icecast on port ${ICECAST_LISTEN_PORT}..."
    if ! su -s /bin/bash -c "icecast2 -c \"$ICECAST_CONFIG\" -b" icecast2; then
        echo "Failed to start Icecast with $ICECAST_CONFIG" >&2
        exit 1
    fi
fi

if [ "${PROCESS_CAP_ALERTS:-true}" = "false" ]; then
    export PROCESS_CAP_ALERTS=false
else
    export PROCESS_CAP_ALERTS=true
fi

php-fpm8.4 -R
nginx
eas_listener
