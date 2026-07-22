# v0.32.0: Released 2026-07-22

- **ARM images are here.** `latest` is now a multi-arch manifest covering `linux/amd64`, `linux/arm64`, and `linux/arm/v7`, so everything from a Raspberry Pi 2 on 32-bit Pi OS to a Pi 5 on a 64-bit OS can pull the image directly with no config change. CI now builds with Buildx + QEMU instead of a plain `docker build`. Thanks to @UrkiMimi who opened the ARM request issue (#6).

- **Speechify Tom now ALSO runs natively on every architecture we publish.** [Speechify](https://github.com/wagwan-piffting-blud/Speechify) native engine release 2026.07.22 adds native `x86_64`, `arm64`, and `armv7` Linux builds, all of which are wired in. Verified: all four published Linux binaries (legacy 32-bit `x86`, `x86_64`, `arm64`, `armv7`) synthesize the same test phrase to *byte-identical* WAV output -- one unique SHA-256 across the whole set. A Raspberry Pi 2 on 32-bit Pi OS produces the same audio, sample for sample, as an x86-64 server, so there is no voice drift between deployments.

- **i386 multiarch is gone from the image.** amd64 now uses the native `spfy-linux-x86_64` build instead of the 32-bit `spfy-linux-x86` one, so `dpkg --add-architecture i386` and `libc6:i386` are no longer installed at all. Architecture selection lives in a `SPFY_ASSET_SLUG_*` / `SPFY_ASSET_SHA256_*` pair per arch, where a non-empty checksum is what switches that architecture on -- so picking up a future architecture is a two-line change, and the i386 branch only fires if an arch is deliberately pointed back at the legacy 32-bit asset.

- **The `-lite` image is deprecated and will stop being published after v0.32.0.** There is now one unified image built from one Dockerfile. This was made possible by a package audit that removed a surprising amount of dead weight: Liquidsoap (every audio path has always used ffmpeg), p7zip-full, tmux, gnupg2, wget, git, and six PHP extensions that no shipped PHP file ever called (`php-mysql`, `php-curl`, `php-gd`, `php-mbstring`, `php-xml`, `php-zip`). With that gone, `latest` and `latest-lite` were within ~20 MB of each other -- the Piper voice model by itself is larger than the entire Speechify payload -- so maintaining two diverging Dockerfiles no longer bought anything. `Dockerfile.lite` and `docker_entrypoint_lite.sh` are deleted; the `-lite` tag is still published from the unified Dockerfile via `--build-arg VARIANT=lite` so existing pulls keep working during the deprecation window.

- **TTS engine is now resolved at startup against what the image actually contains.** Leave `TTS_ENGINE` unset and the entrypoint picks Speechify Tom where it exists and Piper everywhere else. Ask for an engine the image does not have and you get a clear warning in the logs plus an automatic fallback to Piper, instead of a failure on the first CAP alert that needs TTS. `espeak-ng` is now installed in every image too -- it was always a supported `TTS_ENGINE` value in the code but was never actually present in the full image.

- **Dashboard notices.** The dashboard now renders a banner when you are running the deprecated `-lite` image, and a second banner when your requested TTS engine was unavailable and got substituted. Both are driven by an `image_info.json` that the entrypoint writes at every boot, so they need no configuration.

- Speechify Tom voice blobs (`tom.vin`, `tom8.vdb`, `tom.vcf`) are now fetched from a pinned commit over HTTPS and verified with SHA-256, replacing an unpinned shallow `git clone` of `main`. This makes the build reproducible and drops `git` from the runtime image entirely.

- Fixed the OpenSSL runtime dependency, which was named `libssl3`. On Debian trixie that package has no installation candidate on *any* architecture -- it was only resolving on amd64 through virtual-package indirection, and it fails outright on armhf, where Debian's 64-bit `time_t` transition is visible. The image now installs `libssl3t64` by name, which is the real package on amd64, arm64, and armhf alike.

---

v0.31.0: Released 2026-07-22

- Some small changes across the board, linting, comment removal, etc. to reduce the size of the codebase and improve readability.

---

v0.30.0: Released 2026-07-13

- Happy version 30! I am introducing a new CHANGES.md file to keep track of all the changes made in EAS_Listener. This will help maintain a clear history of updates and improvements made to EAS_Listener. EAS Tools uses the same kind of CHANGES.md file to keep track of changes made in EAS Tools. The CHANGES.md file will be updated with each new version, and it will include a summary of the changes made, along with the version number and date of the release.

- Introduce AGENTS.md file to help agentic development of EAS_Listener. This file will contain information about the project to help coding agents understand the project and its goals. It will include details about the architecture, design patterns, and coding standards used in EAS_Listener. The AGENTS.md file will be updated as needed to provide the most up-to-date information for coding agents working on EAS_Listener.

- Reduce Speechify Tom TTS dependency size to only the bare minimum required for the TTS engine to function. This will help reduce the overall size of the project and improve performance. (We also swapped over from Wine to spfy_synth, a NATIVE Linux binary that will run on Linux without Wine, which is a huge improvement for performance and stability. The UID match, however, is still 100% with the real Speechify Tom thanks to the in-line DLL FE loader.)

- Complete Icecast 2 stream integration. This was pending for the longest time, but never finished. Currently, this means that the listener can now output its own stream of alerts only, 24/7. There is a normal alert queue for frequent alert periods. The Icecast RELAY portion is 100% unmodified and works just the same.

- Add "Send Test Alert" button to the EAS_Listener GUI. This allows users to easily test the whole alert system pipeline without having to wait for an actual alert to occur. The test alert will simulate a real alert and will be sent through the same channels as a real alert, allowing users to verify that their setup is working correctly. Helpful if you recently changed something and don't know if your changes will work. The test alert will also be logged in the alert history for reference. Major thanks to GitHub user [@averlice](https://github.com/averlice) for the idea.

- Remove errant "icecast.xml" file from the Dockerfile. This file is not tracked locally and was causing build issues. Icecast should supply its own file.
