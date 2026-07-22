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

# --------------------------------------------------------------------------- #
# TTS engine resolution
#
# Speechify Tom only exists as a 32-bit x86 binary, so it ships in the amd64
# "full" image and nowhere else. Resolve the requested engine against what is
# actually installed so an ARM image (or the deprecated -lite image) degrades to
# Piper at startup instead of failing on the first CAP alert that needs TTS.
# --------------------------------------------------------------------------- #
IMAGE_VARIANT="${EAS_IMAGE_VARIANT:-full}"
IMAGE_ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"

engine_available() {
    case "$1" in
        speechify) command -v spfy_synth >/dev/null 2>&1 ;;
        piper)     command -v piper >/dev/null 2>&1 ;;
        espeak-ng) command -v espeak-ng >/dev/null 2>&1 ;;
        *)         return 1 ;;
    esac
}

REQUESTED_TTS_ENGINE="$(printf '%s' "${TTS_ENGINE:-}" | tr -d '[:space:]')"
TTS_ENGINE_FALLBACK_REASON=""

if [ -z "$REQUESTED_TTS_ENGINE" ]; then
    if engine_available speechify; then
        RESOLVED_TTS_ENGINE="speechify"
    else
        RESOLVED_TTS_ENGINE="piper"
    fi
    echo "TTS engine not configured; auto-selected '${RESOLVED_TTS_ENGINE}' (variant=${IMAGE_VARIANT}, arch=${IMAGE_ARCH})."
elif engine_available "$REQUESTED_TTS_ENGINE"; then
    RESOLVED_TTS_ENGINE="$REQUESTED_TTS_ENGINE"
    echo "TTS engine: ${RESOLVED_TTS_ENGINE} (variant=${IMAGE_VARIANT}, arch=${IMAGE_ARCH})"
else
    RESOLVED_TTS_ENGINE="piper"
    TTS_ENGINE_FALLBACK_REASON="TTS engine '${REQUESTED_TTS_ENGINE}' is not installed in this image (variant=${IMAGE_VARIANT}, arch=${IMAGE_ARCH})"
    echo "WARNING: ${TTS_ENGINE_FALLBACK_REASON}; falling back to 'piper'." >&2
    if [ "$REQUESTED_TTS_ENGINE" = "speechify" ]; then
        echo "WARNING: Speechify Tom ships only where upstream publishes a spfy build for the architecture. Every 'full' image we publish (amd64, arm64, armv7) includes it; it is never included in the -lite image." >&2
    fi
fi

export TTS_ENGINE="$RESOLVED_TTS_ENGINE"

DEPRECATION_NOTICE=""
if [ "$IMAGE_VARIANT" = "lite" ]; then
    DEPRECATION_NOTICE="The -lite image is deprecated and will stop being published after v0.32.0. Switch your image tag to ghcr.io/wagwan-piffting-blud/eas-listener:latest, which now ships Piper, espeak-ng and (on amd64) Speechify Tom in one image and supports arm64. Set TTS_ENGINE explicitly if you want to stay on Piper."
    echo "==================================================================" >&2
    echo "DEPRECATION: ${DEPRECATION_NOTICE}" >&2
    echo "==================================================================" >&2
fi

sed -i "s/session.gc_maxlifetime = .*/session.gc_maxlifetime = 259200/" /etc/php/8.4/fpm/php.ini

chmod -R 777 /app /data /var/www/html

# Surfaced by the dashboard (see web_server/notices.php). Written after the chmod
# above so a bind-mounted /var/www/html still ends up with a readable file.
IMAGE_INFO_PATH="/var/www/html/image_info.json"
if jq -n \
    --arg variant "$IMAGE_VARIANT" \
    --arg arch "$IMAGE_ARCH" \
    --arg tts_engine "$RESOLVED_TTS_ENGINE" \
    --arg tts_engine_requested "$REQUESTED_TTS_ENGINE" \
    --arg tts_engine_fallback_reason "$TTS_ENGINE_FALLBACK_REASON" \
    --arg deprecation_notice "$DEPRECATION_NOTICE" \
    '{variant: $variant, arch: $arch, tts_engine: $tts_engine, tts_engine_requested: $tts_engine_requested, tts_engine_fallback_reason: $tts_engine_fallback_reason, deprecation_notice: $deprecation_notice}' \
    > "${IMAGE_INFO_PATH}.tmp" 2>/dev/null; then
    mv -f "${IMAGE_INFO_PATH}.tmp" "$IMAGE_INFO_PATH"
    chmod 644 "$IMAGE_INFO_PATH" 2>/dev/null || true
else
    rm -f "${IMAGE_INFO_PATH}.tmp" 2>/dev/null || true
    echo "WARNING: could not write ${IMAGE_INFO_PATH}; dashboard notices will be unavailable." >&2
fi

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
