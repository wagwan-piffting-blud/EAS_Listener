#!/bin/bash
set -eu

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
    echo "TTS engine: ${TTS_ENGINE:-piper} (no server startup needed)"
fi

php-fpm8.4 -R
nginx
eas_listener
