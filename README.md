# Rust EAS Listener and Notifier

*A software ENDEC that listens to broadcast audio streams, decodes EAS/SAME messages, records audio, and pushes rich notifications via Apprise with a real-time monitoring dashboard. Also supports Make Your Own DASDEC, relaying to Icecast, and event/FIPS-code based filtering.*

---

![Dashboard image](./dashboard.png)

---

## The `-lite` image is deprecated

**`ghcr.io/wagwan-piffting-blud/eas-listener:latest-lite` will stop being published after v0.32.0.** There is now a single unified image that covers both use cases.

Historically `latest` shipped Speechify Tom and `latest-lite` shipped Piper, because Speechify dragged in a large dependency tree. After pruning a pile of packages that were installed but never invoked (Liquidsoap, p7zip, tmux, gnupg2, wget, and six unused PHP extensions), the two images ended up roughly the same size — the Piper voice model alone is larger than the entire Speechify payload. Keeping two near-identical Dockerfiles in sync stopped being worth it.

**To migrate, change one line in your `docker-compose.yml`:**

```yaml
image: ghcr.io/wagwan-piffting-blud/eas-listener:latest
```

Then, if you want to keep using Piper, set the engine explicitly in your `config.json` (or `.env`):

```json
"TTS_ENGINE": "piper"
```

Five engines are available: `speechify` (Tom), `cepstral` (Cepstral Swift 6.2 -- Allison, David,
Jean-Pierre or William, chosen with `TTS_MODEL`), `loquendo` (Loquendo 6.9 Dave), `piper` and
`espeak-ng`. Piper and espeak-ng are in the image. The other three are not: each lives in its own
repository ([Speechify](https://github.com/wagwan-piffting-blud/Speechify),
[cep6-rs](https://github.com/wagwan-piffting-blud/cep6-rs),
[ENDEC_Dave](https://github.com/wagwan-piffting-blud/ENDEC_Dave)), and the first boot that selects
one downloads that engine from its release onto the state volume, pinned by version and SHA-256 in
[`tools/components.json`](./tools/components.json). Later boots find it there and skip the
download, and a new image with a newer pin replaces it. The Cepstral voices are 80-211 MB each and
arrive the same way, on the first boot that selects `cepstral`.

| Engine | First-boot download | Lands in |
| --- | --- | --- |
| `speechify` | about 80 MB, engine and Tom together | `/data/tools/spfy_synth`, `/data/tts_voices/spfy/voices/tom` |
| `loquendo` | about 36-40 MB, Dave built in | `/data/tools/loqdave` |
| `cepstral` | about 12 MB, plus the chosen voice | `/data/tools/cep6`, `/data/tts_voices/cep6/<voice>` |

If you do not set `TTS_ENGINE`, the container fetches Speechify and uses it, falling back to Piper when that is not possible. Nothing silently breaks — if the engine you asked for cannot be installed, the entrypoint logs why, falls back to Piper, and the dashboard shows a banner explaining what happened. Switching to another of these three engines later needs a container restart, since that is when the download happens.

Outside Docker, `tools/fetch_components.sh` (Linux and macOS) and `tools/fetch_components.ps1` (Windows) install the same pinned builds beside the binary: `fetch_components.sh loqdave`, for example.

Running the deprecated `-lite` tag also raises a banner on the dashboard and a warning in the container logs until you migrate.

---

## Supported architectures

| Platform | Speechify Tom | Cepstral | Loquendo | Piper | espeak-ng | Typical hardware |
| --- | --- | --- | --- | --- | --- | --- |
| `linux/amd64` | ✅ | ✅ | ✅ | ✅ | ✅ | x86-64 servers, NAS, mini PCs |
| `linux/arm64` | ✅ | ✅ | ✅ | ✅ | ✅ | Raspberry Pi 4/5 (64-bit OS), Apple silicon |
| `linux/arm/v7` | ✅ | ✅ | ✅ | ✅ | ✅ | Raspberry Pi 2/3, 32-bit Pi OS |

ARM support is new as of v0.32.0 — `latest` is now a multi-arch manifest, so ARM hosts pull the right image automatically with no config change.

**Every engine is available on every platform.** Speechify release 2026.07.22 ships native `x86_64`, `arm64`, and `armv7` Linux builds, and all of them — plus the legacy 32-bit `x86` build — synthesize *byte-identical* audio. A Raspberry Pi 2 and an x86-64 server produce the same WAV, sample for sample, so there is no voice drift between deployments.

amd64 now uses the native `x86_64` build rather than the legacy 32-bit one, so the image no longer enables i386 multiarch at all.

---

## Features

- Real-time EAS/SAME message decoding from multiple audio sources (primarily Icecast/Shoutcast streams)
- Includes 1050Hz tone detection for the streams you mark as NOAA Weather Radio, for alerts that are not SAME-toned
- Optional CAP alert processing with TTS support for CAP alerts that don't have SAME headers (e.g. NWR/IPAWS CAP alerts)
- Configurable TTS word replacements for CAP alerts to improve readability and pronunciation **(NOTE: does NOT support phoneme codes or SSML tags, only simple word/phrase replacements!)**
- Audio recording and optional Icecast relaying
- Rich notifications via [Apprise](https://github.com/caronc/apprise) and Discord embed support
- Web-based monitoring dashboard, served by the listener itself (no PHP, nginx or separate web server)
- Runs from a Docker container or as a standalone binary — on Windows, optionally with a system-tray icon
- [Make Your Own DASDEC](https://github.com/wagwan-piffting-blud/MYOD/tree/cross-platform-with-audio) support
- Event-code based filtering
- Docker image with everything pre-configured and included
- Highly configurable via JSON
- Modular and extensible architecture
- Written in Rust for ultimate performance and memory safety

---

## Running without Docker

The listener serves the dashboard itself, and the dashboard is built into the binary, so the
executable on its own is a complete install. Drop it somewhere, run it, and the dashboard is on
`MONITORING_BIND_PORT` (8080 by default); first run writes `config.json` beside it.

```
eas-listener/
    eas_listener(.exe)      the binary, dashboard included
    config.json             your configuration, written by first-run setup
    tools/                  ffmpeg and friends (see tools/README.md)
    web_server/             optional: dashboard files to serve instead of the built-in copy
```

A `web_server/` directory next to the executable wins over the built-in copy whenever it holds an
`index.html`, so a source checkout and the Docker image both serve the files on disk and an edit
shows up on the next refresh. `EAS_WEB_ROOT` points that somewhere else. A release archive ships no
`web_server/`, so a released binary always serves what it was built with; the startup log says
which it is:

```
Serving the dashboard source="built into the binary (26 files)"
```

Paths resolve relative to the executable's own directory, or to `EAS_APP_ROOT` when it is set.
`tools/fetch_components.ps1` (Windows) and `tools/fetch_components.sh` (Linux/macOS) download the
pinned third-party binaries the listener needs; see [`tools/README.md`](./tools/README.md) for what
is required, what is optional, and the licensing.

### Prebuilt binaries

Tagging a release (`v*`) builds and publishes one archive per platform, each holding the binary,
this README, the changelog, the licence, the example configuration and the component fetch
scripts:

| Archive | Built on | Notes |
| --- | --- | --- |
| `eas-listener-<version>-linux-x86_64.tar.gz` | Ubuntu 22.04 | glibc 2.35, so Debian 12 and newer run it |
| `eas-listener-<version>-linux-aarch64.tar.gz` | Ubuntu 22.04 (arm64) | |
| `eas-listener-<version>-windows-x86_64.zip` | Windows Server 2022 | built with the `tray` and `service` features |
| `eas-listener-<version>-macos-universal.tar.gz` | macOS 15 (Apple silicon) | one binary for Apple silicon and Intel, with the `tray` feature |
| `eas-listener-<version>-macos-universal.dmg` | macOS 15 (Apple silicon) | the same, signed with a Developer ID and notarized |

The macOS binary is signed and notarized, so Gatekeeper opens it without complaint, and the `.dmg`
carries its notarization ticket for machines that are offline. A release made before signing was
set up has only an unsigned `.tar.gz`; downloaded in a browser, that needs `xattr -dr
com.apple.quarantine eas-listener-*` before macOS will run it.

`SHA256SUMS` is published alongside them:

```bash
sha256sum -c SHA256SUMS --ignore-missing     # shasum -a 256 -c SHA256SUMS on macOS
```

The release notes come from this repository's `CHANGES.md` section for that version. The workflow
also runs the test suite and starts each binary once before publishing anything.

### First run

Start the listener with no `config.json` (or an empty one) and it does not start on built-in
defaults. It serves a setup page instead, opens it in your default browser, and prints its address
with a one-time token:

```
http://127.0.0.1:8080/setup.html?token=...
```

The token is also written to `setup-token.txt` next to `config.json`; in Docker, `docker logs
eas_listener` shows it. Setup asks for dashboard sign-in details, the streams to monitor, your time
zone and locations, whether to poll CAP, and, optionally, where alerts should be announced. Only
what you fill in is written. Everything else keeps its default without being copied into the file,
and can be changed later from the dashboard.

The browser is only opened where someone can see it: on Windows (but not from the Windows
service), and on Linux when there is a graphical session (`DISPLAY` or `WAYLAND_DISPLAY`), so never
from Docker, a systemd unit or a plain SSH session. `--no-browser` or `EAS_NO_BROWSER=1` turns it
off.

With Docker, if `./config.json` does not exist before the first `docker compose up`, Docker mounts
an empty directory in its place. That works: the configuration is saved inside it, as
`config.json/config.json`, and read from there from then on.

A `config.json` that is present but broken -- invalid JSON, or a value the listener rejects -- stops
it from starting, with the reason on the console (and, for the Windows service, in
`service-error.log` next to `config.json`). It never falls back to built-in defaults. A reload of a
broken file is refused the same way, and the listener keeps the configuration it already has.

### Editing the configuration

The dashboard has a **Edit configuration** link that opens `config.json` as a form, with every
setting described, grouped and searchable, and the raw JSON one click away. Edits are checked with
the same rules the listener applies at startup, so an invalid file is rejected before it can
replace a working one, and the previous version is copied to `config.json.bak` on every save.
**Save & Reload** applies the change without restarting, apart from the few settings the form
marks as read at startup.

Every setting can be written in `config.json` or set as an environment variable, and behaves the
same either way. When a key is given in both, the environment wins, except in Docker, where
`config.json` wins so that edits made from the dashboard take effect. The form shows which settings
an environment variable is affecting.

`MONITORING_BIND_ADDR` chooses the interface and `MONITORING_BIND_PORT` chooses the port. Setting
either one alone works; if both are set and disagree, the explicit port wins, so the address only
contributes its interface.

### Where alerts are announced

Notifications go to the URLs in `apprise.yml` (next to `config.json`, or wherever
`APPRISE_CONFIG_PATH` points). The configuration page and first-run setup both edit it under
**Notifications**:

- **Add a service** lists every service the installed Apprise supports, more than 150, and builds
  the URL from a form with that service's own fields, checked as you type. Private values such as
  tokens and passwords are hidden in the list and the preview.
- **Paste a URL** takes any Apprise URL as is, for anything the form does not cover.
- **Send a test** sends a short message to one URL, saved or not, and reports what came back.

Discord webhooks (`discord://`) are sent by the listener itself, with an embed and the recording
attached, and work without Apprise. Every other service needs Apprise; `tools/fetch_components`
installs it. A saved list applies from the next alert, with no reload. Saving keeps the previous
file as `apprise.yml.bak` and writes one URL per line, so YAML keys or tags in a hand-written file
are dropped; the page says so before you save.

As with `config.json`, if `./apprise.yml` does not exist before the first `docker compose up`,
Docker mounts a directory in its place, and the list is saved inside it.

### Reloading without the dashboard

`POST /api/reload` and `POST /api/test-alert` are what the dashboard uses, but the older signal
files still work for scripts and cron jobs — `touch` one next to `config.json` and it fires within
a second:

```
touch reload_signal        # re-read config.json and apply it
touch test_alert_signal    # inject a narrated Required Weekly Test
```

The listener consumes each file by deleting it, so a second `touch` is a second request.

### Testing a TTS engine

**Send Test Alert** narrates the test alert with whichever `TTS_ENGINE` is configured, through
exactly the path a CAP alert without audio takes, so you don't have to wait for one to find out
an engine is broken. To compare engines, change `TTS_ENGINE`/`TTS_MODEL` in the configuration
editor, **Save & Reload**, and send another test — tests are never dropped as duplicates, however
close together they are. The log says how each one went:

```
Test alert TTS OK with loquendo: 11.6s of 16000 Hz audio in 2.8s.
Test alert TTS FAILED with piper: Failed to spawn Piper TTS process: program not found.
```

The recording in the archive has the narration between the header and the NNNN. A test whose
engine fails still goes out, just without narration.

A standalone install ships no TTS engine. The one `TTS_ENGINE` names is downloaded, with its voice,
right after the listener starts or reloads, so switching engines and pressing **Save & Reload** is
all it takes; the log shows the download. What each platform can fetch:

| Engine | Windows | Linux | macOS |
| --- | --- | --- | --- |
| piper (the default) | ✅ | ✅ | ❌ upstream's macOS build is broken |
| loquendo, cepstral | ✅ | ✅ | ✅ one universal build, signed and notarized |
| speechify | ✅ the 32-bit engine, with Tom | ✅ | ✅ signed and notarized |
| espeak-ng | ❌ install the `.msi` | ✅ from the distro | ✅ from Homebrew |

The Cepstral voice is a 337 MB download the first time. An engine that cannot be fetched says why
in the log and in the test alert's result.

### TTS replacements

A built-in dictionary maps abbreviations to what should be said — `"Hwy ": "Highway "`,
`"9-1-1": "nine one one"` — whether or not you add your own; it is
`cap_tts_replacement_config.example.json`, compiled in. A `cap_tts_replacement_config.json` next to
`config.json` adds to it, and wins where both name the same key. `TTS_BUILTIN_REPLACEMENTS: false`
turns the built-in one off, leaving only yours. One dictionary serves every engine, so stick to plain
words and respellings (`"Pot-a-wat-a-mee"`); an engine's own control codes would be read aloud by
the others.

The sender at the end of the opening sentence -- `(KWO35)`, `Message from KWO35` -- is left out of
what is read aloud, in every ENDEC mode; `TTS_READ_CALLSIGN: true` reads it. With `TTS_ENGINE` `loquendo` and `ENDEC_MODE` `SAGE`, the opening
sentence also goes through the same rewrite a SAGE 3644's HelloTTS front end applies before its own
Loquendo speaks: a one-second pause first, state codes spelled out (`N.E,`), weekdays and months
in full, and the sender never read -- as on the real unit.

It applies to everything spoken for a CAP alert: the opening sentence, the description and the
instructions. URLs and hashtags are left alone for the listener to spell out. Keys match whole
words only, so `"S "` changes "5 MI S of" but not "HAS ISSUED", and the longest key wins where
several could match. Case follows the key: an all-lowercase key like `"hwy "` also matches "HWY ",
while a key with capitals like `"LA"` matches exactly. The file is read for every alert, so edits
take effect without a restart. Set `RUST_LOG` to `DEBUG` to log the exact text each engine is given.

### The 24/7 alert stream

With `ICECAST_ALERT_STREAM_ENABLED` on, the listener serves a continuous Ogg Vorbis stream itself --
comfort noise between alerts, and each alert's audio as it happens -- at
`http://<host>:ICECAST_ALERT_PORT` + `ICECAST_ALERT_MOUNT` (`:8000/stream.ogg` by default), on the
same interface as the dashboard. No Icecast server is involved any more: that was the only thing it
did, and the URL it served is unchanged, so players and automation already pointed at it keep
working. A listener who connects mid-stream is sent the stream's headers first and starts decoding
at once. In Docker, publish the port in the compose file. `ICECAST_ALERT_HOST` and the
`ICECAST_ALERT_SOURCE_*` settings are gone; an Icecast server elsewhere that should carry the stream
can pull it with a `<relay>`. Relaying alerts *to* someone else's Icecast (`ICECAST_RELAY`) is
unchanged.

Where ffmpeg has no `libvorbis` encoder -- Homebrew's ffmpeg on macOS is built without it -- the
stream is Ogg Opus instead, at the same URL, and the log says so at startup.

### Marking NOAA Weather Radio streams

NOAA Weather Radio precedes an alert with a 1050 Hz tone instead of a SAME header, so the listener
watches for one and records when it hears it. Anything else with a steady tone near 1050 Hz — a
test signal, some station idents, a bit of music — can set that off, so which streams are listened
to is up to you. Tick **NWR** beside a stream in the configuration form, or list the URLs by hand:

```json
"ICECAST_STREAM_URL_ARRAY": [
    "https://wxr.gwes-cdn.net/KIH61",
    "https://icecast.example.com/scanner.mp3"
],
"ICECAST_STREAM_NWR": [
    "https://wxr.gwes-cdn.net/KIH61"
]
```

Here the tone starts a recording on the weather radio feed and is ignored on the scanner. SAME
decoding is untouched either way: every stream is decoded for headers, marked or not.

With `ICECAST_STREAM_NWR` absent from config.json, every stream is watched — what the listener did
before this setting existed, so an upgrade changes nothing until you narrow it. An empty list means
no stream is. The log says which it is for each stream as it connects:

```
Watching for the 1050 Hz NOAA Weather Radio tone stream=https://wxr.gwes-cdn.net/KIH61
Not marked as NOAA Weather Radio, so the 1050 Hz tone is ignored on this stream stream=https://icecast.example.com/scanner.mp3
```

The setting is read per chunk of audio, so ticking or clearing a stream takes effect on the next
reload without restarting the listener.

### CAP-CP alerts and the Alert Ready tone

By default a CAP-CP (Alert Ready / NAAD) alert is recorded the way every other alert is: SAME
header, attention tone, the message, NNNN. Set `CAPCP_USE_ALERT_READY_TONE` to `true` to record
them the Canadian way instead:

```
[Alert Ready attention signal, 8 s] [1 s silence] [the message]
```

The attention signal plays once, at the start, and nothing follows the message — no closing
tone, no NNNN. Because nothing about the alert went out as SAME, the SAME header is also left out
of every webhook (Discord embeds, Markdown, HTML and plain-text Apprise bodies). The dashboard and
archive still show it, since it is how the listener identifies the alert internally.

The option only affects CAP-CP alerts; IPAWS CAP alerts and alerts decoded off the air keep their
SAME framing. A DASDEC relay still receives the SAME header, because the endpoint needs one to
encode; an Icecast relay plays the recording as-is, so it carries the Alert Ready tone.

### Turning the header tones off, or replacing them

Set `EMIT_HEADER_TONES` to `false` and a recording holds the message audio alone:

```
[the message]
```

No SAME header, no attention tone, no NNNN, and no Alert Ready signal even with
`CAPCP_USE_ALERT_READY_TONE` on. It applies to every alert, on the air and from CAP alike. The
NNNN a station sends off the air is still trimmed off the end of what was captured, so nothing of
the original tones is left either. For a CAP alert this also keeps the SAME header out of
webhooks, the way the Alert Ready framing does, since no SAME went out for it.

`CUSTOM_HEADER_AUDIO` replaces the tones an alert opens with instead of removing them. Point it at
any audio file ffmpeg can read:

```
[your header audio] [1 s silence] [the message] [1 s silence] [NNNN]
```

It stands in for the SAME header burst and the attention tone, or for the Alert Ready signal when
that framing is in use. Only the opening is replaced: the NNNN that ends a SAME-framed recording
stays. This is separate from `ICECAST_INTRO` and `ICECAST_OUTRO`, which still play outside it when
they are turned on. A file that is missing or unreadable when the alert arrives is logged and the
generated tones are used for that recording. `EMIT_HEADER_TONES` set to `false` wins: nothing
opens the recording, so the custom audio is not played either.

### Starting at boot: a Windows service, a systemd unit or a launchd job

First-run setup asks, on its last page, whether the listener should start with the computer. Say
yes and it installs itself, starts that way, and hands over to it; the process you ran by hand
then exits. On Windows it asks for administrator permission first (if the page is open on another
machine, where that prompt could not be answered, it shows the command to run instead). On Linux it
installs directly when it is running as root, and otherwise shows the `sudo` command. On macOS it
always installs directly: without root it sets up a LaunchAgent for your account. Docker never
asks: set `restart: unless-stopped` on the container instead.

The same thing from a terminal, at any time:

```
eas_listener.exe --install-service          # Windows, from an administrator terminal
sudo ./eas_listener --install-service       # Linux, or macOS for a LaunchDaemon
./eas_listener --install-service            # macOS, a LaunchAgent for your account
```

Each one registers it to start automatically and starts it now; stop a copy you started by hand
first, since both cannot hold the port. `--uninstall-service` removes it (with `sudo` for a
LaunchDaemon) and `--service-status` reports its state.

| | Windows | Linux | macOS, with `sudo` | macOS, without |
| --- | --- | --- | --- | --- |
| What is installed | the `EASListener` service, automatic start | `/etc/systemd/system/eas-listener.service`, enabled | a LaunchDaemon in `/Library/LaunchDaemons` | a LaunchAgent in `~/Library/LaunchAgents` |
| Starts | at boot | at boot | at boot, before anyone logs in | when you log in |
| Runs as | LocalSystem | the account that owns the install folder (root only if root owns it) | the account that owns the install folder | you, with the menu bar icon in a `tray` build |
| Restarts after a crash | after 60 s, 60 s, then every 5 minutes | after 10 s, giving up after 5 failures in 10 minutes | after 10 s | after 10 s |
| Logs | `service-error.log` beside `config.json` when it stops on its own | `journalctl -u eas-listener -f` | `launchd.log` beside `config.json` | `launchd.log` beside `config.json` |
| Build | Windows releases include the `service` feature; from source, `--features service` | any build | any build | any build |

The Linux unit and the macOS job are written for the install they came from -- the executable,
the folder and its owner -- so there is no template to edit. Move the install and run
`--install-service` again to rewrite them. The macOS job also puts Homebrew's folders on its
`PATH`, so an ffmpeg installed with `brew` is found the same way it is from a terminal. As a
service, the listener keeps its alert database and recordings in `data/` beside the executable
unless `SHARED_STATE_DIR` says otherwise, because a service's temporary folder is not yours:
LocalSystem's on Windows, one that may be cleared at boot on Linux, and one macOS clears after a few
days.

### Desktop tray icon

An optional build puts a tray icon on the desktop -- in the menu bar on macOS, with no Dock icon --
with **Open Dashboard**, **Open Log Folder** and **Quit**:

```
cargo build --release --features tray
```

The feature is off by default, so container and headless builds are unaffected. A `tray` build
started with no desktop to show the icon on -- a systemd unit, a LaunchDaemon, an SSH session --
runs without one. If the listener itself stops, the icon goes with it and the process exits with
an error, so a service manager watching it restarts it.

### macOS

The listener runs natively on Apple silicon and Intel Macs. ffmpeg comes from Homebrew
(`brew install ffmpeg`, or `tools/fetch_components.sh`, which runs that for you), and the fetcher
installs Apprise from apprise-go's macOS builds. Homebrew's ffmpeg has no `libvorbis`, so the alert
stream is Ogg Opus there instead of Ogg Vorbis. Starting automatically is a launchd job; see the
table above.

---

## Installation, configuration, usage, technical details

[Please refer to the wiki](https://github.com/wagwan-piffting-blud/EAS_Listener/wiki) for detailed instructions on installation, configuration, usage, and more that this README cannot cover in-depth.

---

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The dashboard will check for updates on GitHub and notify users when a new version is available. Patch versions are for bug fixes and minor improvements to the dashboard, minor versions are for new features or bug fixes to the listener itself, and major versions are mostly unused (per Rust ecosystem tradition). Please refer to the project's commit history for detailed changes and updates.

---

## License

This project is licensed under the **GNU GPL-3.0** (see [`LICENSE`](LICENSE)).

---

## Acknowledgments
- [\@\_spchalethorpe09\_](https://sterlingvaspc.neocities.org/) and [\@aimaismog](https://github.com/aimaismog) on Discord for thorough testing, feedback, and suggestions
- Global Weather and EAS Society (GWES) for their overall support and resources
- SAME decoders and EAS/NWR community research
- Rust ecosystem maintainers

## GenAI Disclosure Notice: Portions of this repository have been generated using Generative AI tools (ChatGPT, ChatGPT Codex, GitHub Copilot).
