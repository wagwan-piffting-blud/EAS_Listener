#!/bin/bash
set -eu

ORIGINAL_LOCAL_DEEPLINK_HOST="${LOCAL_DEEPLINK_HOST:-}"

# /tmp survives `docker restart`, so the healthcheck would otherwise probe the last run's address
# until this run's server binds and rewrites it.
if [ -n "${EAS_HEALTH_ADDR_FILE:-}" ]; then
    rm -f "$EAS_HEALTH_ADDR_FILE"
fi

convert_config_to_env() {
    local config_file="$1"
    local env_file="$2"
    local prefix="${3:-}"

    jq -r 'to_entries | .[] | "'"${prefix}"'" + (.key | ascii_upcase) + "=" + (if (.value | type) == "string" then .value else (if (.value | type) == "array" then (.value | @json) else (.value | tostring) end) end)' "$config_file" >> "$env_file"
}

CONFIG_JSON=/app/config.json
# Docker mounts a directory when ./config.json does not exist on the host yet. The listener keeps
# its configuration inside it then, and so does this.
if [ -d "$CONFIG_JSON" ]; then
    CONFIG_JSON="${CONFIG_JSON}/config.json"
fi

# A missing or blank config.json is a first run: the listener serves its setup page instead of
# starting, so there is nothing to export yet. One that is present but broken stops here, and the
# listener would refuse it anyway rather than start on built-in defaults.
: > /app/.env
if [ -f "$CONFIG_JSON" ] && [ -n "$(tr -d '[:space:]' < "$CONFIG_JSON")" ]; then
    if ! jq -e 'type == "object"' "$CONFIG_JSON" >/dev/null; then
        echo "ERROR: ${CONFIG_JSON} is not a valid JSON object. Fix it, or empty it to run first-time setup." >&2
        exit 1
    fi
    convert_config_to_env "$CONFIG_JSON" /app/.env ""
else
    echo "No configuration in ${CONFIG_JSON} yet; the listener will start in first-run setup mode."
fi

# The copy above is for this script's own decisions. The listener reads config.json directly, and
# this makes config.json outrank that boot-time copy, so an edit is picked up by a reload.
export EAS_CONFIG_PRECEDENCE=file
FORCED_KEYS=""

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
    FORCED_KEYS="LOCAL_DEEPLINK_HOST"
fi

IMAGE_VARIANT="${EAS_IMAGE_VARIANT:-full}"
IMAGE_ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"

engine_available() {
    case "$1" in
        speechify) command -v spfy_synth >/dev/null 2>&1 ;;
        piper)     command -v piper >/dev/null 2>&1 ;;
        espeak-ng) command -v espeak-ng >/dev/null 2>&1 ;;
        cepstral)  command -v cep6 >/dev/null 2>&1 ;;
        loquendo)  command -v loqdave >/dev/null 2>&1 ;;
        *)         return 1 ;;
    esac
}

REQUESTED_TTS_ENGINE="$(printf '%s' "${TTS_ENGINE:-}" | tr -d '[:space:]')"
TTS_ENGINE_FALLBACK_REASON=""

# The state volume, resolved the way the listener resolves it: relative to /app, its working
# directory. /data when unset, because that is the volume every compose file mounts.
STATE_DIR="${SHARED_STATE_DIR:-/data}"
case "$STATE_DIR" in
    /*) ;;
    *)  STATE_DIR="/app/${STATE_DIR}" ;;
esac

# cep6, loqdave and Speechify are not in the image. The engine config.json asks for -- or
# Speechify, which is what an unset TTS_ENGINE picks -- is fetched onto the state volume on the
# first boot that needs it, pinned and checksummed by /app/tools/components.json. Later boots find
# it and skip, and a release taken down upstream costs a failed download here rather than a
# rebuilt image.
export PATH="${STATE_DIR}/tools:${PATH}"
fetch_engine() {
    if ! /app/tools/fetch_components.sh -d "$STATE_DIR" "$1"; then
        echo "WARNING: the '$1' TTS engine could not be installed into ${STATE_DIR}/tools." >&2
    fi
}
case "${REQUESTED_TTS_ENGINE:-speechify}" in
    speechify)
        if [ "$IMAGE_VARIANT" = "full" ]; then
            fetch_engine speechify
        fi
        ;;
    cepstral)
        fetch_engine cep6
        ;;
    loquendo)
        fetch_engine loqdave
        ;;
esac
# A fallback only: a SPFY_VOICE_DIR from config.json was exported above and is left alone.
if [ -z "${SPFY_VOICE_DIR:-}" ] && [ -d "${STATE_DIR}/tts_voices/spfy/voices/tom" ]; then
    export SPFY_VOICE_DIR="${STATE_DIR}/tts_voices/spfy/voices/tom"
fi

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
    TTS_ENGINE_FALLBACK_REASON="TTS engine '${REQUESTED_TTS_ENGINE}' is not available (variant=${IMAGE_VARIANT}, arch=${IMAGE_ARCH})"
    echo "WARNING: ${TTS_ENGINE_FALLBACK_REASON}; falling back to 'piper'." >&2
    if [ "$REQUESTED_TTS_ENGINE" = "speechify" ] && [ "$IMAGE_VARIANT" != "full" ]; then
        echo "WARNING: Speechify Tom is never installed by the -lite image." >&2
    fi
fi

# The Cepstral voices are half a gigabyte, so they are not shipped in the image. The first boot
# on this engine pulls the configured one onto the state volume; later boots find it and skip.
if [ "$RESOLVED_TTS_ENGINE" = "cepstral" ]; then
    CEP6_VOICE="${TTS_MODEL:-Allison}"
    CEP6_DIR="${CEP6_VOICE_DIR:-${STATE_DIR}/tts_voices/cep6}"
    if /app/tts_voices/cep6/fetch_voices.sh -d "$CEP6_DIR" "$CEP6_VOICE"; then
        export CEP6_VOICE_DIR="$CEP6_DIR"
    else
        RESOLVED_TTS_ENGINE="piper"
        TTS_ENGINE_FALLBACK_REASON="the Cepstral voice '${CEP6_VOICE}' could not be installed in ${CEP6_DIR}"
        echo "WARNING: ${TTS_ENGINE_FALLBACK_REASON}; falling back to 'piper'." >&2
    fi
fi

export TTS_ENGINE="$RESOLVED_TTS_ENGINE"
if [ -n "$REQUESTED_TTS_ENGINE" ] && [ "$RESOLVED_TTS_ENGINE" != "$REQUESTED_TTS_ENGINE" ]; then
    FORCED_KEYS="${FORCED_KEYS:+${FORCED_KEYS},}TTS_ENGINE"
fi
export EAS_CONFIG_FORCED_KEYS="$FORCED_KEYS"

DEPRECATION_NOTICE=""
if [ "$IMAGE_VARIANT" = "lite" ]; then
    DEPRECATION_NOTICE="The -lite image is deprecated and will stop being published after v0.32.0. Switch your image tag to ghcr.io/wagwan-piffting-blud/eas-listener:latest, which now ships Piper, espeak-ng and (on amd64) Speechify Tom in one image and supports arm64. Set TTS_ENGINE explicitly if you want to stay on Piper."
    echo "==================================================================" >&2
    echo "DEPRECATION: ${DEPRECATION_NOTICE}" >&2
    echo "==================================================================" >&2
fi

chmod -R 777 /app /data

IMAGE_INFO_PATH="/app/image_info.json"
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

if [ "${PROCESS_CAP_ALERTS:-true}" = "false" ]; then
    export PROCESS_CAP_ALERTS=false
else
    export PROCESS_CAP_ALERTS=true
fi

# First-run setup exits with 75 once config.json is saved, so everything above runs again with it.
export EAS_RESTART_AFTER_SETUP=1
set +e
eas_listener
LISTENER_STATUS=$?
set -e
if [ "$LISTENER_STATUS" -eq 75 ]; then
    echo "First-run setup saved config.json; running the container's startup again to apply it."
    exec "$0" "$@"
fi
exit "$LISTENER_STATUS"
