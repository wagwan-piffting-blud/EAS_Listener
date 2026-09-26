#!/bin/bash
# Fetches Cepstral voice data for the cep6 engine into a writable directory. The engine binaries
# are vendored in git; the voices are not, because all four together are half a gigabyte.
#
#   fetch_voices.sh -d /data/tts_voices/cep6 Allison David
#
# Voices already present are left alone, so this is safe to run on every boot.
set -eu
# Readable by every listener on the machine, whichever account fetched the voices.
umask 022

ARCHIVE_NAME="DASDEC_Cepstral_Voices.7z"
DEFAULT_URL="https://uploads.wagspuzzle.space/${ARCHIVE_NAME}"
DEFAULT_SHA256="a43312ae597ca5078c584ad0bc4048be82ab07b528c45dfc6dee9b359ccba3a4"
KNOWN_VOICES="Allison David Jean-Pierre William"

TARGET_DIR="${CEP6_VOICE_DIR:-}"
ARCHIVE_URL="${CEP6_VOICE_ARCHIVE_URL:-$DEFAULT_URL}"
EXPECTED_SHA256="${CEP6_VOICE_ARCHIVE_SHA256:-$DEFAULT_SHA256}"
KEEP_ARCHIVE=false

usage() {
    echo "usage: fetch_voices.sh [-d DIR] [-u URL] [-s SHA256|skip] [-k] VOICE [VOICE...]"
    echo
    echo "  -d DIR     where the voices are installed. Default: \$CEP6_VOICE_DIR"
    echo "  -u URL     archive to fetch. Default: \$CEP6_VOICE_ARCHIVE_URL, else"
    echo "             ${DEFAULT_URL}"
    echo "  -s SHA256  expected archive digest, or 'skip' to accept any. Default:"
    echo "             \$CEP6_VOICE_ARCHIVE_SHA256, else the digest of the published archive"
    echo "  -k         keep the downloaded archive instead of deleting it after extraction"
    echo
    echo "Voices: ${KNOWN_VOICES}"
}

log() {
    echo "cep6: $*"
}

die() {
    echo "cep6: $*" >&2
    exit 1
}

find_7z() {
    local candidate
    for candidate in 7zz 7z 7za 7zr p7zip; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return 0
        fi
    done
    # libarchive's tar reads .7z too: it is the system tar on macOS and the BSDs, and bsdtar
    # where a Linux distribution packages it.
    for candidate in bsdtar tar; do
        if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version 2>/dev/null | grep -q libarchive; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# 7-Zip and libarchive's tar spell "extract these paths into DIR" differently.
extract_voice() {
    local tool="$1" archive="$2" dest="$3" voice="$4"
    case "$tool" in
        *tar) "$tool" -xf "$archive" -C "$dest" "${voice}/*" ;;
        *)    "$tool" x -y -bso0 -bsp0 -o"$dest" "$archive" "${voice}/*" ;;
    esac
}

is_known_voice() {
    local wanted="$1" known
    for known in $KNOWN_VOICES; do
        [ "$wanted" = "$known" ] && return 0
    done
    return 1
}

# A voice is only usable with all three of these present; a half-extracted directory is not
# treated as installed, so an interrupted run recovers on the next one.
voice_installed() {
    local dir="$1"
    [ -s "${dir}/settings.txt" ] && [ -s "${dir}/voice.idx" ] && [ -s "${dir}/voice_a.dat" ]
}

while getopts ':d:u:s:kh' opt; do
    case "$opt" in
        d) TARGET_DIR="$OPTARG" ;;
        u) ARCHIVE_URL="$OPTARG" ;;
        s) EXPECTED_SHA256="$OPTARG" ;;
        k) KEEP_ARCHIVE=true ;;
        h) usage; exit 0 ;;
        :) die "-$OPTARG needs an argument." ;;
        *) usage >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))

[ "$#" -gt 0 ] || { usage >&2; exit 2; }
[ -n "$TARGET_DIR" ] || die "No target directory. Pass -d DIR or set CEP6_VOICE_DIR."

WANTED=""
for voice in "$@"; do
    is_known_voice "$voice" || die "Unknown voice '${voice}'. Known voices: ${KNOWN_VOICES}"
    if voice_installed "${TARGET_DIR}/${voice}"; then
        log "${voice} is already installed in ${TARGET_DIR}."
        continue
    fi
    WANTED="${WANTED}${WANTED:+ }${voice}"
done

if [ -z "$WANTED" ]; then
    exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required to fetch the voice archive."
SEVENZIP="$(find_7z)" || die "Nothing here can open a .7z. Install the 'p7zip-full' or '7zip' package (or bsdtar)."

mkdir -p "$TARGET_DIR"
WORK_DIR="${TARGET_DIR}/.incoming"
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"
trap 'rm -rf "$WORK_DIR"' EXIT

ARCHIVE_PATH="${CEP6_VOICE_ARCHIVE_PATH:-}"
if [ -n "$ARCHIVE_PATH" ]; then
    [ -s "$ARCHIVE_PATH" ] || die "CEP6_VOICE_ARCHIVE_PATH=${ARCHIVE_PATH} does not exist."
    log "using the local archive at ${ARCHIVE_PATH}"
else
    ARCHIVE_PATH="${WORK_DIR}/${ARCHIVE_NAME}"
    log "fetching ${ARCHIVE_URL} for: ${WANTED}"
    log "this is a ~337 MB download and runs once; the archive is discarded afterwards."
    curl -fL --retry 5 --retry-delay 2 -o "$ARCHIVE_PATH" "$ARCHIVE_URL" \
        || die "Could not download ${ARCHIVE_URL}."
fi

if [ "$EXPECTED_SHA256" != "skip" ] && [ -n "$EXPECTED_SHA256" ]; then
    if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL_SHA256="$(sha256sum "$ARCHIVE_PATH" | cut -d' ' -f1)"
        else
            ACTUAL_SHA256="$(shasum -a 256 "$ARCHIVE_PATH" | cut -d' ' -f1)"
        fi
        if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
            die "Archive digest ${ACTUAL_SHA256} does not match the expected ${EXPECTED_SHA256}. Pass -s <digest> for a re-uploaded archive, or -s skip to accept it unchecked."
        fi
    else
        log "sha256sum is unavailable; skipping the digest check."
    fi
fi

STAGE_DIR="${WORK_DIR}/stage"
mkdir -p "$STAGE_DIR"

for voice in $WANTED; do
    log "extracting ${voice}"
    extract_voice "$SEVENZIP" "$ARCHIVE_PATH" "$STAGE_DIR" "$voice" \
        || die "Could not extract ${voice} from ${ARCHIVE_PATH}."
    voice_installed "${STAGE_DIR}/${voice}" \
        || die "${voice} came out of the archive incomplete."
    rm -rf "${TARGET_DIR}/${voice}"
    mv "${STAGE_DIR}/${voice}" "${TARGET_DIR}/${voice}"
    chmod -R a+rX "${TARGET_DIR}/${voice}"
    log "installed ${voice} in ${TARGET_DIR}/${voice}"
done

if [ "$KEEP_ARCHIVE" = "true" ] && [ -z "${CEP6_VOICE_ARCHIVE_PATH:-}" ]; then
    mv "$ARCHIVE_PATH" "${TARGET_DIR}/${ARCHIVE_NAME}"
    log "kept the archive at ${TARGET_DIR}/${ARCHIVE_NAME}"
fi
