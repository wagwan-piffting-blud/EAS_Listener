#!/bin/bash
set -eu

export ALREADY_SET_UP=false

if [ $(grep -c 'env\[MONITORING_BIND_PORT\]' /etc/php/8.4/fpm/pool.d/www.conf) -gt 0 ]; then
    export ALREADY_SET_UP=true
fi

if [ ! ${ALREADY_SET_UP:-} = "true" ]; then
    convert_config_to_env() {
        local config_file="$1"
        local env_file="$2"
        local prefix="${3:-}"

        jq -r 'to_entries | .[] | "'"${prefix}"'" + (.key | ascii_upcase) + "=" + (if (.value | type) == "string" then .value else (if (.value | type) == "array" then (.value | @json) else (.value | tostring) end) end)' "$config_file" >> "$env_file"
    }

    eval "$(convert_config_to_env /app/config.json /app/.env "")"

    sed -i '/^FILTERS=/d' /app/.env
    export $(grep -v '^#' /app/.env | xargs)

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
    su -www-data -s /bin/bash -c "printf 'env[MONITORING_MAX_LOGS] = ${MONITORING_MAX_LOGS}\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[WATCHED_FIPS] = \"${WATCHED_FIPS:-}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[TZ] = \"${TZ}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[ALERT_SOUND_SRC] = \"${ALERT_SOUND_SRC}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    su -www-data -s /bin/bash -c "printf 'env[ALERT_SOUND_ENABLED] = \"${ALERT_SOUND_ENABLED}\"\n' >> /etc/php/8.4/fpm/pool.d/www.conf"
    sed -i "s/session.gc_maxlifetime = .*/session.gc_maxlifetime = 259200/" /etc/php/8.4/fpm/php.ini

    export ALREADY_SET_UP=true
fi

chmod -R 777 /app /data /var/www/html

php-fpm8.4 -R
nginx
eas_listener
