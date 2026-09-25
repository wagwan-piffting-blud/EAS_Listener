#!/bin/bash
# Fetches the pinned third-party binaries listed in components.json into this directory.
#
# The listener resolves each binary as: config.json key, then this directory, then PATH. Anything
# placed here is found automatically with no configuration.
#
#   fetch_components.sh              install everything pinned for this platform
#   fetch_components.sh -l           list what the manifest knows about
#   fetch_components.sh -f ffmpeg    re-download one component
#   fetch_components.sh -d /data cep6
#                                    install into /data/tools (and /data/<data dirs>) instead of
#                                    beside this script -- what the Docker entrypoint does
#
# Downloads are verified against the SHA-256 digests published by each upstream. Nothing is
# redistributed by this project; the files come from their own projects' servers.
set -eu

TOOLS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${TOOLS_DIR}/components.json"
INSTALL_ROOT="$(dirname "$TOOLS_DIR")"
FORCE=false
LIST=false
ONLY=""

log()  { echo "components: $*"; }
warn() { echo "components: $*" >&2; }
die()  { echo "components: $*" >&2; exit 1; }

# jq.exe under Git Bash opens stdout in text mode and appends carriage returns, which would end up
# inside filenames. Every read of the manifest goes through here so that cannot happen.
jqr() { jq -r "$@" "$MANIFEST" | tr -d '\r'; }

usage() {
    echo "usage: fetch_components.sh [-l] [-f] [-d ROOT] [COMPONENT]"
    echo
    echo "  -l        list the components in the manifest and exit"
    echo "  -f        re-download even if the binaries are already present"
    echo "  -d ROOT   install binaries into ROOT/tools and data into ROOT"
    echo "            (default: the directory above this script)"
}

while getopts ":lfhd:" opt; do
    case "$opt" in
        l) LIST=true ;;
        f) FORCE=true ;;
        d) INSTALL_ROOT="$OPTARG" ;;
        h) usage; exit 0 ;;
        *) usage >&2; exit 1 ;;
    esac
done
shift $((OPTIND - 1))
if [ $# -gt 0 ]; then
    ONLY="$1"
fi
BIN_DIR="${INSTALL_ROOT}/tools"

command -v jq >/dev/null 2>&1 || die "jq is required to read the manifest"
command -v curl >/dev/null 2>&1 || die "curl is required to download components"
[ -f "$MANIFEST" ] || die "manifest not found at $MANIFEST"

# The manifest keys platforms the way rustc names targets, so the same file serves every OS.
case "$(uname -s)" in
    Linux)                        OS="linux" ;;
    Darwin)                       OS="macos" ;;
    MINGW*|MSYS*|CYGWIN*)         OS="windows" ;;
    *)                            OS="$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
esac
case "$(uname -m)" in
    x86_64|amd64)   ARCH="x86_64" ;;
    aarch64|arm64)  ARCH="aarch64" ;;
    armv7l|armv7)   ARCH="armv7" ;;
    *)              ARCH="$(uname -m)" ;;
esac
PLATFORM="${OS}-${ARCH}"
# The manifest stores bare binary names; Windows needs the extension appended.
if [ "$OS" = "windows" ]; then EXE=".exe"; else EXE=""; fi

if [ "$LIST" = true ]; then
    log "platform: ${PLATFORM}"
    jqr '.components | to_entries[] | [.key, .value.requirement, .value.license, .value.install] | @tsv' \
        | while IFS=$'\t' read -r key requirement license install; do
            printf '  %-10s %-9s %-20s %s\n' "$key" "$requirement" "$license" "$install"
        done
    exit 0
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "neither sha256sum nor shasum is available"
    fi
}

pinned_sha() {
    jqr --arg c "$1" --arg p "$PLATFORM" '.components[$c].platforms[$p].sha256 // empty'
}

# Records which pinned download a component came from, so bumping the pin in the manifest
# replaces what an earlier run installed instead of keeping it because the files exist.
stamp_path() {
    echo "${BIN_DIR}/.${1}.sha256"
}

already_installed() {
    local component="$1" binary target stamp prefix=""
    [ "$(jqr --arg c "$component" '.components[$c].layout // empty')" = "directory" ] \
        && prefix="${component}/"
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        [ -f "${BIN_DIR}/${prefix}${binary}${EXE}" ] || return 1
    done < <(jqr --arg c "$component" '.components[$c].provides[]')
    while IFS= read -r target; do
        [ -n "$target" ] || continue
        [ -s "${INSTALL_ROOT}/${target}" ] || return 1
    done < <(component_files "$component" | cut -f3)
    while IFS= read -r target; do
        [ -n "$target" ] || continue
        [ -d "${INSTALL_ROOT}/${target}" ] || return 1
    done < <(jqr --arg c "$component" '.components[$c].data // [] | .[].to')
    # No stamp means an install from before stamps existed; its files are trusted as they are.
    stamp="$(stamp_path "$component")"
    if [ -f "$stamp" ] && [ "$(tr -d '[:space:]' < "$stamp")" != "$(pinned_sha "$component")" ]; then
        log "${component} is pinned to a different build than the one installed; replacing it"
        return 1
    fi
    return 0
}

# Downloads the pinned file for this platform into $2 and checks its digest. Returns 2 when
# nothing is pinned for the platform, which the caller reports as a manual install.
download_pinned() {
    local name="$1" dest_dir="$2" url expected version sha
    url="$(jqr --arg c "$name" --arg p "$PLATFORM" '.components[$c].platforms[$p].url // empty')"
    expected="$(pinned_sha "$name")"
    version="$(jqr --arg c "$name" '.components[$c].version // "unknown"')"

    if [ -z "$url" ]; then
        warn "${name} has no build pinned for ${PLATFORM}; install it yourself and set its *_PATH key"
        return 2
    fi

    DOWNLOADED="${dest_dir}/$(basename "$url")"
    log "downloading ${name} ${version}"
    log "  ${url}"
    curl -fL --retry 3 --retry-delay 2 -o "$DOWNLOADED" "$url" || { warn "  download failed"; return 1; }

    sha="$(sha256_of "$DOWNLOADED")"
    if [ "$sha" != "$expected" ]; then
        warn "SHA-256 mismatch for $(basename "$url")"
        warn "  expected ${expected}"
        warn "  actual   ${sha}"
        return 1
    fi
    log "  sha256 verified"
    return 0
}

# A release asset that is the executable itself, with no archive around it.
install_binary() {
    local name="$1" tmp binary
    tmp="$(mktemp -d)"
    # A RETURN trap outlives the function that set it, and `tmp` is local, so it clears itself.
    trap 'rm -rf "$tmp"; trap - RETURN' RETURN

    # Not `set +e` around the call: that would switch errexit back on inside the caller's own
    # `set +e`, and the next non-zero return would end the whole script.
    download_pinned "$name" "$tmp" || return $?

    binary="$(jqr --arg c "$name" '.components[$c].provides[0]')"
    mkdir -p "$BIN_DIR"
    install -m 0755 "$DOWNLOADED" "${BIN_DIR}/${binary}${EXE}"
    log "  installed ${binary}${EXE}"
    return 0
}

install_archive() {
    local name="$1" archive tmp extract found binary from to member
    archive="$(jqr --arg c "$name" --arg p "$PLATFORM" '.components[$c].platforms[$p].archive // empty')"

    tmp="$(mktemp -d)"
    # A RETURN trap outlives the function that set it, and `tmp` is local, so it clears itself.
    trap 'rm -rf "$tmp"; trap - RETURN' RETURN

    # Not `set +e` around the call: that would switch errexit back on inside the caller's own
    # `set +e`, and the next non-zero return would end the whole script.
    download_pinned "$name" "$tmp" || return $?

    extract="${tmp}/extracted"
    mkdir -p "$extract"
    case "$archive" in
        tar.xz) tar -xJf "$DOWNLOADED" -C "$extract" ;;
        tar.gz) tar -xzf "$DOWNLOADED" -C "$extract" ;;
        zip)    command -v unzip >/dev/null 2>&1 || die "unzip is required for ${name}"
                unzip -q "$DOWNLOADED" -d "$extract" ;;
        *)      warn "unsupported archive type '${archive}' for ${name}"; return 1 ;;
    esac

    # The layout inside upstream archives is not part of their contract, so search for the
    # binaries rather than assuming a bin/ subdirectory. `archive_member` covers an archive whose
    # one executable is named for its platform rather than for what it provides.
    member="$(jqr --arg c "$name" --arg p "$PLATFORM" '.components[$c].platforms[$p].archive_member // empty')"
    mkdir -p "$BIN_DIR"

    # A program that needs the libraries and data shipped beside it keeps its whole folder, as
    # BIN_DIR/<name>/, rather than having its executable lifted out on its own.
    if [ "$(jqr --arg c "$name" '.components[$c].layout // empty')" = "directory" ]; then
        binary="$(jqr --arg c "$name" '.components[$c].provides[0]')"
        found="$(find "$extract" -type f -name "${binary}${EXE}" -print -quit)"
        if [ -z "$found" ]; then
            warn "  ${binary}${EXE} was not present in the archive"
            return 1
        fi
        rm -rf "${BIN_DIR:?}/${name}"
        cp -R "$(dirname "$found")" "${BIN_DIR}/${name}"
        chmod -R a+rX "${BIN_DIR}/${name}"
        chmod 0755 "${BIN_DIR}/${name}/${binary}${EXE}"
        log "  installed ${name}/ with ${binary}${EXE}"
        return 0
    fi

    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        found="$(find "$extract" -type f -name "${member:-${binary}${EXE}}" -print -quit)"
        if [ -z "$found" ]; then
            warn "  ${binary}${EXE} was not present in the archive"
            return 1
        fi
        install -m 0755 "$found" "${BIN_DIR}/${binary}${EXE}"
        log "  installed ${binary}${EXE}"
    done < <(jqr --arg c "$name" '.components[$c].provides[]')

    # Directories the component needs beside its binaries -- a voice, say. Matched by the end of
    # their path for the same reason the binaries are searched for.
    while IFS=$'\t' read -r from to; do
        [ -n "$from" ] || continue
        found="$(find "$extract" -type d -path "*/${from}" -print -quit)"
        if [ -z "$found" ]; then
            warn "  ${from}/ was not present in the archive"
            return 1
        fi
        rm -rf "${INSTALL_ROOT:?}/${to}"
        mkdir -p "$(dirname "${INSTALL_ROOT}/${to}")"
        cp -R "$found" "${INSTALL_ROOT}/${to}"
        chmod -R a+rX "${INSTALL_ROOT}/${to}"
        log "  installed ${to}/"
    done < <(jqr --arg c "$name" '.components[$c].data // [] | .[] | [.from, .to] | @tsv')

    return 0
}

# Single files the component needs from somewhere other than its own download -- Piper's voice
# model, say -- each pinned by digest and installed under INSTALL_ROOT.
install_files() {
    local name="$1" url sha to tmp actual
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"; trap - RETURN' RETURN
    while IFS=$'\t' read -r url sha to; do
        [ -n "$url" ] || continue
        if [ -s "${INSTALL_ROOT}/${to}" ] && [ "$FORCE" != true ]; then
            continue
        fi
        log "  downloading ${to}"
        curl -fL --retry 3 --retry-delay 2 -o "${tmp}/file" "$url" || { warn "  download failed: ${url}"; return 1; }
        actual="$(sha256_of "${tmp}/file")"
        if [ "$actual" != "$sha" ]; then
            warn "  SHA-256 mismatch for ${to}: expected ${sha}, got ${actual}"
            return 1
        fi
        mkdir -p "$(dirname "${INSTALL_ROOT}/${to}")"
        mv "${tmp}/file" "${INSTALL_ROOT}/${to}"
        chmod a+r "${INSTALL_ROOT}/${to}"
        log "  installed ${to}"
    done < <(component_files "$name")
    return 0
}

# The component's own files plus the platform's: a platform whose download lacks what the others'
# archives carry -- Speechify's bare Windows executable, without Tom -- lists it itself.
component_files() {
    jqr --arg c "$1" --arg p "$PLATFORM" \
        '(.components[$c].files // []) + (.components[$c].platforms[$p].files // [])
         | .[] | [.url, .sha256, .to] | @tsv'
}

has_system_fallback() {
    { [ "$OS" = "linux" ] || [ "$OS" = "macos" ]; } && [ -z "$(pinned_sha "$1")" ] \
        && [ -n "$(jqr --arg c "$1" '.components[$c].system_packages // empty | keys[]')" ]
}

# Homebrew's own directory is only on PATH in a login shell that ran `brew shellenv`, which an
# SSH command or a script started from launchd has not.
if [ "$OS" = "macos" ]; then
    for brew_bin in /opt/homebrew/bin /usr/local/bin; do
        if [ -x "${brew_bin}/brew" ]; then
            case ":${PATH}:" in
                *":${brew_bin}:"*) ;;
                *) PATH="${brew_bin}:${PATH}" ;;
            esac
            break
        fi
    done
fi

# The system's own build, for a platform nothing is pinned for: the distro's package on Linux,
# Homebrew's on macOS. It lands on PATH, which the listener searches last, so nothing needs
# configuring.
install_system() {
    local name="$1" binary manager package="" sudo=""
    binary="$(jqr --arg c "$name" '.components[$c].provides[0]')"
    if command -v "$binary" >/dev/null 2>&1 && [ "$FORCE" != true ]; then
        log "${name}: using the system ${binary} at $(command -v "$binary")"
        return 0
    fi

    for manager in apt-get dnf apk pacman zypper brew; do
        command -v "$manager" >/dev/null 2>&1 || continue
        package="$(jqr --arg c "$name" --arg m "$manager" '.components[$c].system_packages[$m] // empty')"
        [ -n "$package" ] && break
    done
    if [ -z "$package" ]; then
        if [ "$OS" = "macos" ]; then
            warn "${name} has no build pinned for ${PLATFORM}; install Homebrew (https://brew.sh), then run this again"
        else
            warn "${name} has no build pinned for ${PLATFORM} and no supported package manager was found"
        fi
        return 2
    fi
    if [ "$manager" = "brew" ]; then
        # Homebrew refuses to run as root, and needs nothing from it.
        if [ "$(id -u)" -eq 0 ]; then
            warn "${name}: Homebrew does not run as root; run this again without sudo"
            return 2
        fi
    elif [ "$(id -u)" -ne 0 ]; then
        if ! command -v sudo >/dev/null 2>&1; then
            warn "${name} has no build pinned for ${PLATFORM}; as root, install the ${package} package with ${manager}"
            return 2
        fi
        sudo="sudo"
    fi

    log "${name} has no build pinned for ${PLATFORM}; installing the system's ${package} with ${manager}"
    case "$manager" in
        brew)    HOMEBREW_NO_AUTO_UPDATE=1 brew install "$package" ;;
        apt-get) $sudo apt-get update -qq \
                     && $sudo env DEBIAN_FRONTEND=noninteractive \
                        apt-get install -y -qq --no-install-recommends "$package" ;;
        dnf)     $sudo dnf install -y -q "$package" ;;
        apk)     $sudo apk add --no-cache "$package" ;;
        pacman)  $sudo pacman -S --noconfirm --needed "$package" ;;
        zypper)  $sudo zypper --non-interactive install "$package" ;;
    esac || { warn "  ${manager} could not install ${package}"; return 1; }

    command -v "$binary" >/dev/null 2>&1 \
        || { warn "  ${package} installed, but ${binary} is not on PATH"; return 1; }
    log "  installed ${binary} at $(command -v "$binary")"
    return 0
}

write_provenance() {
    local sources="${BIN_DIR}/SOURCES.txt" name
    {
        echo "Third-party binaries installed in this directory."
        echo "Recorded $(date '+%Y-%m-%d %H:%M:%S %z') on ${PLATFORM}."
        echo
        echo "These were downloaded from their own upstream projects and are NOT redistributed"
        echo "by EAS_Listener. Each remains under its own license, shown below."
        echo
        for name in "$@"; do
            jqr --arg c "$name" --arg p "$PLATFORM" '
                .components[$c] as $x
                | "\($c)  (\($x.license))",
                  "  version:  \($x.version // "n/a")",
                  "  project:  \($x.project_url)",
                  "  source:   \($x.source_url)",
                  "  url:      \($x.platforms[$p].url // "n/a")",
                  "  sha256:   \($x.platforms[$p].sha256 // "n/a")",
                  "  note:     \($x.license_note)",
                  ""
            '
        done
    } > "$sources"
    log "provenance written to ${sources}"
}

if [ -n "$ONLY" ]; then
    jq -e --arg c "$ONLY" '.components | has($c)' "$MANIFEST" >/dev/null 2>&1 \
        || die "unknown component '${ONLY}'. Known: $(jqr '.components | keys | join(", ")')"
    NAMES="$ONLY"
else
    NAMES="$(jqr '.components | keys[]')"
fi

INSTALLED=()
SYSTEM=()
MANUAL=()
FAILED=()

for name in $NAMES; do
    install_type="$(jqr --arg c "$name" '.components[$c].install')"
    feature="$(jqr --arg c "$name" '.components[$c].feature // ""')"

    if has_system_fallback "$name"; then
        set +e
        install_system "$name"
        rc=$?
        set -e
        case "$rc" in
            0) SYSTEM+=("$name") ;;
            2) MANUAL+=("$name") ;;
            *) FAILED+=("$name") ;;
        esac
        continue
    fi

    case "$install_type" in
        archive|binary)
            if already_installed "$name" && [ "$FORCE" != true ]; then
                log "${name} is already present; pass -f to re-download"
                INSTALLED+=("$name")
                continue
            fi
            # A platform's own download decides: the same component can be a bare executable on
            # one platform and an archive on another, whatever its install type says.
            platform_archive="$(jqr --arg c "$name" --arg p "$PLATFORM" '.components[$c].platforms[$p].archive // empty')"
            set +e
            if [ -z "$platform_archive" ]; then
                install_binary "$name"
            else
                install_archive "$name"
            fi
            rc=$?
            if [ "$rc" -eq 0 ]; then
                install_files "$name"
                rc=$?
            fi
            set -e
            case "$rc" in
                0) INSTALLED+=("$name")
                   pinned_sha "$name" > "$(stamp_path "$name")" ;;
                2) MANUAL+=("$name") ;;
                *) FAILED+=("$name") ;;
            esac
            ;;
        manual|system)
            warn "${name} must be installed manually."
            jqr --arg c "$name" --arg p "$PLATFORM" \
                '.components[$c].platforms[$p].installer_url // empty
                 , .components[$c].platforms[$p].instructions // empty' \
                | sed 's/^/    /'
            echo "    Without it: ${feature}"
            MANUAL+=("$name")
            ;;
        *)
            warn "${name} has an unrecognised install type '${install_type}'; skipping"
            ;;
    esac
done

if [ ${#INSTALLED[@]} -gt 0 ]; then
    write_provenance "${INSTALLED[@]}"
fi

echo
log "installed: ${INSTALLED[*]:-none}"
[ ${#SYSTEM[@]} -gt 0 ] && log "from the system's packages: ${SYSTEM[*]}"
[ ${#MANUAL[@]} -gt 0 ] && log "needs manual install: ${MANUAL[*]}"
if [ ${#FAILED[@]} -gt 0 ]; then
    warn "failed: ${FAILED[*]}"
    exit 1
fi
exit 0
