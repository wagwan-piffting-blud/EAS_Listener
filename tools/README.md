# Third-party components

The listener shells out to a handful of external binaries. This directory is where a non-Docker
install keeps them.

## How a binary is found

Each component resolves independently, in this order:

1. An explicit path in `config.json` (`FFMPEG_PATH`, `APPRISE_PATH`,
   `PIPER_PATH`, `ESPEAK_NG_PATH`, `SPFY_SYNTH_PATH`, `CEP6_PATH`, `LOQDAVE_PATH`).
2. This directory, which every instance of the listener shares.
3. `tools/` in the instance's own folder, where the listener fetches instead when the account
   running it cannot write here (an install in `/opt` that root unpacked, say).
4. `PATH`.

`fetch_components.sh -d ROOT` and `fetch_components.ps1 -InstallRoot ROOT` install into
`ROOT/tools` instead of here; that is how the listener points them at an instance's folder.

`PATH` is last so Docker and distro installs keep using their packaged binaries — putting nothing
here changes nothing about those deployments.

The listener probes every component at startup and on each config reload. The results are logged
and served from `GET /api/components`.

## Fetching them

```
# Windows
.\fetch_components.ps1            # install everything pinned for this platform
.\fetch_components.ps1 -List      # show what the manifest knows about
.\fetch_components.ps1 -Force     # re-download even if already present

# Linux / macOS  (needs jq and curl)
./fetch_components.sh
./fetch_components.sh -l
./fetch_components.sh -f ffmpeg
./fetch_components.sh -d /data loqdave    # into /data/tools instead of here
```

Downloads are checked against the SHA-256 digests in `components.json`, which were taken from the
digest GitHub reports for each release asset. A mismatch aborts that component.

## Updating a pin

A component hosted on GitHub names its repository (`github`), its `release_tag`, and one `asset`
per platform; the download URL is implied. `{tag}`, `{version}` (the `release_tag` unless the
component gives its own) and `{github}` are substituted in any of its strings. Bumping one is a
single command, never a hand edit:

```
python tools/update_components.py loqdave                 # newest release with every platform's asset
python tools/update_components.py loqdave -t 2026.10.06   # that release
python tools/update_components.py -n --all                # show what bumping everything would change
python tools/update_components.py --check                 # every pin still matches GitHub?
```

It rewrites `release_tag`, `version` and each `sha256` from GitHub's asset digests, downloading
and hashing only what GitHub has no digest for (piper's 2023 assets) and `files` whose URL depends
on the tag (Speechify's Tom files for Windows). It never uses GitHub's "Latest" marker, which
ENDEC_Dave keeps on its SAPI zip; "newest" is the most recently published release whose tag
matches the component's `track` regex, if it has one, and that carries every asset. ffmpeg tracks
`^autobuild-` and takes its `version` from the asset names. BtbN publishes nothing but autobuilds
and deletes daily ones after 14 days, keeping the last of each month for two years, so ffmpeg also
sets `"retained": "month-end"`: only the last build of a finished month is ever pinned.
`GH_TOKEN` or `GITHUB_TOKEN` is sent when set.

`.github/workflows/components.yml` runs this daily and opens one pull request per component with a
newer release, on the branch `components/<name>`, after fetching the new linux-x86_64 build with
`fetch_components.sh` to prove it installs. It can be run by hand for one component and tag, or
started from an upstream release with a `component-release` repository dispatch. Its `check` job
fails if any pinned digest stops matching GitHub's.

Each install leaves `.<component>.sha256` beside the binaries, naming the build it came from. When a
newer `components.json` pins a different build, the next run replaces the old one instead of
keeping it because the file exists. An install from before these stamps is left alone.

No ffmpeg build is pinned for ARMv7 Linux or for macOS, so there the script installs the system's
`ffmpeg` instead and the listener finds it on `PATH`: the distro's package on Linux (apt-get, dnf,
apk, pacman or zypper, through `sudo` when not run as root), and Homebrew's on macOS (`brew install
ffmpeg`, which must not be run as root). An `ffmpeg` already on `PATH` -- or in Homebrew's own
folder, which a non-login shell leaves off `PATH` -- is used as it is.

A platform whose download is an archive holding one oddly named executable -- apprise-go's macOS
zips hold `apprise-go-darwin-arm64` -- names it with `archive_member`, and it is installed under
the component's own name.

**The listener runs this script itself.** A missing ffmpeg is fetched at startup, and the TTS
engine `TTS_ENGINE` names -- with its voice -- right after startup and each reload, and again if
it is still missing when an alert needs it. Nothing is fetched over a path set in `config.json`,
and a fetch that fails is not retried for 10 minutes. The output lands in the listener's log.

Piper is pinned for Windows and Linux, installed as a whole folder (`tools/piper/`, since it needs
the libraries beside it; a component with `"layout": "directory"`), and its default voice model is
fetched into `piper/` as one of the component's `files`. Its upstream macOS archives are broken (an
x86_64 build missing `libespeak-ng`), so macOS has no Piper pin. espeak-ng comes from the system's
packages on Linux and macOS; on Windows it is the `.msi` from its releases page.

Cepstral's voices come from `tts_voices/cep6/fetch_voices.sh` (or `.ps1` on Windows), which the
listener runs the same way. The archive is a `.7z`: 7-Zip opens it, and so does libarchive's `tar`,
which is the system `tar` on Windows and macOS and `bsdtar` on Linux.

The three hand-built TTS engines are pinned here too, each from its own repository's release:

| Component | Engine | Repository | Platforms | Also installs |
|---|---|---|---|---|
| `speechify` | `spfy_synth` | [Speechify](https://github.com/wagwan-piffting-blud/Speechify) | Windows (32-bit), Linux x86_64, aarch64, armv7, macOS | the Tom voice, into `tts_voices/spfy/voices/tom` |
| `cep6` | `cep6` | [cep6-rs](https://github.com/wagwan-piffting-blud/cep6-rs) | Windows, Linux x86_64, aarch64, armv7, macOS (universal) | nothing; voices come from `tts_voices/cep6/fetch_voices.sh` |
| `loqdave` | `loqdave` | [ENDEC_Dave](https://github.com/wagwan-piffting-blud/ENDEC_Dave) | Windows, Linux x86_64, aarch64, armv7, macOS (universal) | nothing; Dave is built in |

The macOS builds are signed with a Developer ID and notarized, so Gatekeeper runs them as
downloaded. Speechify's Windows release is the bare engine, with no Tom beside it, so its platform
entry lists Tom's five files under its own `files`, fetched from the repository at the release's
tag. A platform's `files` are added to the component's, and whether a download is unpacked follows
the platform's `archive`, not the component's `install` type.

The Docker image carries none of them. Its entrypoint runs `fetch_components.sh -d /data` for the
engine `TTS_ENGINE` selects, so a release that disappears upstream means a failed download on the
next fresh volume, not a published image that has to be pulled.

After a successful run the script writes `SOURCES.txt` recording exactly what was installed — URL,
version, digest and license. That file is the provenance record; keep it with any copy you pass on.

## What is required

| Component | Required | Without it |
|---|---|---|
| `ffmpeg` | **yes** | nothing works — recording, relay, CAP audio and the alert stream all use it |
| `apprise` | no | no non-Discord notifications; Discord still works, it is sent natively |
| TTS engines | no | that engine is unavailable for CAP alert speech |

A missing **required** component stops startup with a message naming it, rather than failing later
at the first recording.

## Licensing

EAS_Listener is GPL-3.0. Every component here runs as a **separate process** over argv and stdio;
none is linked in. That is mere aggregation under GPLv3 §5 and GPLv2 §2, so their licenses do not
reach into this project's own.

| Component | License | Note |
|---|---|---|
| FFmpeg | LGPL-3.0-or-later | The pinned build is an LGPL build (`--enable-version3`, no `--enable-gpl`) |
| Apprise (apprise-go) | BSD-2-Clause | Permissive, same licence as upstream Apprise |
| Speechify, loqdave | GPL-3.0 | Per their repositories; the voice data inside is the original vendors' |
| cep6 | none declared | The cep6-rs repository carries no licence file |

Two things worth keeping straight:

**The LGPL FFmpeg build is sufficient, deliberately.** The only encoders this project asks for are
`libmp3lame` and `libopus` (see `RecordingFormat::ffmpeg_codec_args` in `src/config.rs`).
`--enable-gpl` is only needed for x264/x265/xvid, which are never used. Never substitute an
`--enable-nonfree` build — those cannot be redistributed at all, by anyone.

**Fetching is not redistributing.** These scripts download from each project's own servers, so the
user obtains the binaries directly from upstream and EAS_Listener never distributes them. That
keeps the source-offer obligations of the GPL and LGPL off this project entirely.

If you instead **bundle** these binaries into a release you hand out, those obligations do attach:
ship each component's license text, keep `SOURCES.txt`, and make the corresponding source available
from the same place as the binaries — for a GitHub release, attach the source archives to that same
release so "same place" is unambiguous. (This is the mechanics as the licenses describe them, not
legal advice.)

## Notes

The pinned FFmpeg binary is static and about 130 MB, because upstream builds them with
every optional library enabled. Almost none of it is used here. If the download size matters for a
packaged installer, a custom minimal build (`--enable-libmp3lame --enable-libopus` and little else)
lands closer to 20 MB — at the cost of building and hosting it yourself, which reintroduces the
redistribution obligations above.

## Apprise

The fetched Apprise is [apprise-go](https://github.com/unraid/apprise-go), Unraid's Go port: one
static binary of about 13 MB per platform, installed as `tools/apprise`, with no Python needed. It
takes the same command line as the Python Apprise, and that command line is all the listener uses,
so an `apprise` from pip, pipx or a distro package on `PATH` (or set as `APPRISE_PATH`) works just
as well.

One difference shows with plain-text services (SMS, text email and the like): the listener sends
its markdown body first, and apprise-go strips the markdown syntax for those services where Python
Apprise passes the asterisks and underscores through as typed.

To move to a newer release, run `python tools/update_components.py apprise`.

## ffprobe and Icecast are no longer needed

ffprobe was required for one thing: finding out what a relay destination's mount already plays.
The listener reads that itself now, with symphonia. Icecast served the 24/7 alert stream; the
listener serves it itself now, on the same port and mount, so `ICECAST_PATH` and the Icecast source
settings are gone. A remote Icecast server that should carry the stream can pull it with a
`<relay>` pointed at `http://<listener>:<ICECAST_ALERT_PORT><ICECAST_ALERT_MOUNT>`.
