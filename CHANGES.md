# EAS Listener Changelog

## v0.43.0: Released 2026-10-10

- **Reduce error/log spam in a short window of time.**

---

## v0.42.0: Released 2026-10-06

- **Fix CAP-CP alert parsing for when the audio element is not truly audio.** Some alerts from Alert Ready Canada have been observed to have an attachment that is not audio, but rather something like a suspect/victim image. This caused the listener to fail to parse the alert and not send the audio to the webhooks. The listener now checks the MIME type of the attachment and only processes it if it is audio, otherwise it will ignore it and continue processing the alert as normal (falling back to TTS with no audio present like it should). Thank you to Discord user dom_navaan for reporting this issue.

- **Fix the component fetcher to handle updates better.** The component fetcher is responsible for downloading and updating the various components of EAS_Listener, such as TTS engines and voices. Previously, it was quite tedious to update manually, requiring me to update each hash by hand and component link and tag. Now, the component fetcher will automatically check for updates and download them if available with the help of a Python script and GitHub action to open PRs when components are updated outside of this repository. This will make it easier to keep EAS_Listener up to date with the latest components and ensure that users have access to the latest features and improvements.

## v0.41.0: Released 2026-09-26

- **Add per-webhook routing based on origin and event code.** This allows users to configure different webhooks for different alert sources and event codes, providing more granular control over how alerts are handled and where they are sent. This feature is particularly useful for users who want to route specific types of alerts to different endpoints or services, without using more than one instance of EAS_Listener. The Listener also now downloads Apprise (specifically a Golang port by the Unraid team, called [apprise-go](https://github.com/unraid/apprise-go), so Python and its suite of dependencies is not dragged in) for non-Discord webhooks, if any are added or used.

- **Add support for multiple EAS Listener instances on one machine.** This allows users to run multiple instances of EAS_Listener on the same machine, each with its own configuration and alert handling. This is useful for users who want to separate different alert sources or event codes into different instances, or for users who want to run multiple instances for testing or development purposes. _NOTE: This will MIGRATE your existing installed data to a new location, specifically, from beside the executable into `<data folder>/eas-listener/default`. The migration will rename your existing files to ".migrated" extensions to prevent confusion._ `<data folder>` is `%ProgramData%` on Windows, `~/.local/share` or `/var/lib` on Linux depending on the user and permissions, and `~/Library/Application Support` or `/Library/Application Support` on macOS depending on root. A service already installed from the old folder keeps using the files beside the executable until it's uninstalled and reinstalled (for example, by running `--uninstall-service` then `--install-service` immediately afterwards).

- **Add the ability to customize ONLY CAP-CP alert tones.** This allows users to configure different opening audio for just CAP-CP alerts, providing more flexibility in how alerts are presented to you/webhooks. The relevant configuration option is `CAPCP_CUSTOM_HEADER_AUDIO`.

- **Support uninstalling EAS_Listener directly from the tray icon and web dashboard.** This allows users to easily uninstall EAS_Listener without having to manually remove files or configurations. The uninstall process will remove that instance's service, config.json, apprise.yml, backups, and logs. The data itself (recorded alerts) are not removed by default, but you do have the option to delete the data as well. If no other instance is left, you can also choose to remove EAS_Listener itself, along with the tools and voices it fetched (TTS engines, apprise-go, etc.). There is a shortcut to the dashboard to perform the uninstall from the tray icon.

- **Change the default archive nesting for EAS_Listener releases.** This means you will simply see the files you need at the top level of the download, rather than inside a nested folder. This change makes it easier to extract EAS_Listener and get started with it, without having to open or extract an extra, unnecessary folder. Inside eas-listener-<version>-<platform>.ARCHIVE-FORMAT (with <version> being the version number and <platform> being the platform, and ARCHIVE-FORMAT being either .tar.gz or .zip), you will find the following files and folders at the top level:

```text
.
├── eas_listener.exe            (or just "eas_listener" on Linux/macOS)
├── README.md
├── CHANGES.md
├── LICENSE
├── config.example.json
├── cap_tts_replacement_config.example.json
├── example.env
├── tools/
└── tts_voices/
```

- **Fix some minor issues and bugs related to the rewrite (v0.40.0 below).**

---

## v0.40.0: Released 2026-09-25

- **This change reworks EAS_Listener from the ground up.** A bunch of changes have been made to the codebase, including a complete rewrite of a fair share of the core functionality of the Rust backend, with the following cherry picked highlights for this release due to the sheer volume of changes made:

  - **PHP is GONE.** The entire frontend has been rewritten to use Rust Axum, which is faster, more efficient, and more secure than relying on PHP code. This change also removes the need for a web server, as the Rust backend can serve the dashboard directly via pure HTML/JS/CSS.

  - **The ability to run EAS_Listener standalone is now possible.** This means that EAS_Listener can now be run without the need for Docker, which previously was the only supported way to run the software. This change makes it easier for users to run EAS_Listener on their own systems, without the need for an entire containerization platform/virtualization support just to receive alerts.

  - **Windows and macOS users now have a tray icon, and all platforms now contain service persistence.** This means that EAS_Listener can now run in the background on Windows, Linux, and macOS, and users can access it from the system tray on Windows and macOS. The service persistence feature ensures that EAS_Listener will start automatically when the system boots up, making it more convenient for users to receive alerts without having to manually start the application on boot. Docker users can still use the Docker container, just like before (it will remain updated as well), but now you have the option to run EAS_Listener natively on the three major operating system platforms.

  - **CAP-CP (Common Alerting Protocol - Canadian Profile) is now supported, alongside Canadian SAME/GeoToCLC locale codes.** This means that EAS_Listener can now receive and fully process alerts from Canadian sources, such as NAAD/Alert Ready. This change expands the reach of EAS_Listener to a wider audience, and allows users in Canada to receive alerts from their own local authorities. NOTE: By default, only "immediate broadcast" alerts are processed (so only those disseminated on TV and radio), but this can be changed in the configuration file to allow for any and all alerts to be processed by using the `CAPCP_REQUIRE_IMMEDIATE` configuration option in your config file. Credit to [ApatheticDELL and QDEC](https://github.com/ApatheticDELL/QDEC) for the GeoToCLC CSV file, which was borrowed verbatim.

  - **More TTS engines now work with EAS_Listener.** EAS_Listener now supports more TTS engines, meaning you can now use any of the following for CAP text to speech fallback capability:

    - **Speechify - any voice** (Tom is the default for both Speechify and EAS_Listener overall, you can find a full list of every voice available [here](https://github.com/wagwan-piffting-blud/Speechify/releases/tag/voices))
    - **Piper**
    - **espeak-ng**
    - **Cepstral - any voice** (new)
    - **Loquendo Dave - ENDEC version** (new)

  - **Full E2T-NG ENDEC profile support/customization.** Previously, EAS_Listener only had the "default"/custom, verbose ENDEC profile, but now it supports all of the E2T-NG ENDEC profiles as a configuration option, which means that users can now choose from a wider range of output text styles for received alert texts. See [E2T-NG](https://github.com/wagwan-piffting-blud/E2T-NG) for more information on the various ENDEC profiles and what they look like in practice.

  - **A new setup and management experience that guides you.** This is a new feature that guides users through the setup process from the first time you run the listener, making it easier to get started with EAS_Listener. The setup experience will walk you through a majority of the configuration options and help you set up your system for receiving alerts. This is a lot simpler than having to manually edit the configuration file, and it will help users get up and running with EAS_Listener more quickly and easily.

That is **most** of the changes for this release! If I left something out, that is my error and I apologize for any possible oversight, but this is 90% of the bulk of the changes made this release. As a personal aside, I want to thank everyone who has supported my development of EAS_Listener and all of my other personal projects over the last few months. I have been working on this and a number of other projects for a fair bit of time now, especially within the EAS community, and it remains a labor of love to those who can't afford things like hardware ENDECs or want to run their own alerting system without relying on a third party or expensive hardware bought second-hand. I hope that this release makes EAS_Listener even more accessible and useful to a wider audience, and I look forward to continuing to improve the software in the future.

- **Minor workflow updates.**

---

## v0.34.0/v.0.35.0: Released 2026-09-06

- **Fix URL parsing for Speechify TTS in CAP descriptions and instructions.** A new URL normalization step has been added to CAP parsing, which ensures that URLs in the `<description>` and `<instruction>` elements are properly formatted for TTS reading. This prevents issues where URLs were being read incorrectly (especially in Speechify). As a result, some spfy/Speechify changes have been made upstream to augment this.

- **The build tracks the latest Speechify release instead of a hard-coded one.** `SPFY_VERSION` now defaults to `latest` and is resolved against the GitHub release feed at build time, and each asset's SHA-256 comes from the release metadata rather than three checksums pasted into the Dockerfile -- the `sha256:` prefix GitHub prints on the release page is stripped automatically, so a version bump is no longer an edit at all. The `SPFY_ASSET_SHA256_*` build args survive as overrides for builds that cannot reach the API, and accept the prefixed or bare form interchangeably. Architecture support is now gated on `SPFY_ASSET_SLUG_*` alone, making a new arch a one-line change. CI resolves the release tag once in the `setup` job and passes it to every matrix leg, which keeps all three architectures on the same release and invalidates the registry buildcache exactly when a new release lands.

- **Update unit tests.** The unit tests have been updated this commit to fix a failed test. This also contained a version number bump in Cargo.toml.

---

## v0.33.0: Released 2026-08-31

- **Updated the Docker build.** The Dockerfile is now updated with recent upstream Speechify/spfy changes. As well, it has been optimized.

- **CAP instructions now reach the dashboard.** The `<instruction>` block of a CAP alert was being parsed and written to the database, but it was never part of the live alert payload the backend pushes over the WebSocket, so the "CAP Instructions" block in the dashboard could never render. `EasAlertData` now carries an `instructions` field alongside `description`, populated from the CAP `<instruction>` element and put through the same whitespace/marker sanitization as the description.

- **Active alerts restored from an older state file now get their CAP fields backfilled.** `active_alerts.json` is written with whatever fields the running build knows about, and the CAP poller skips any alert whose dedupe key is already in the persisted active set -- so an alert carried across an upgrade would sit on the dashboard missing every newly added field until it expired. The skip path now patches the parsed description and instructions onto the matching active alert when they are absent, persists the state file, and rebroadcasts to the dashboard. Values that are already present are never overwritten.

- **The archive shows CAP description and instructions too.** Archived cards previously stopped at the raw ZCZC string. `archive.php` already carried `description` through from the database and now carries `instructions` as well, and `archive.js` renders both blocks on CAP-sourced alerts (detected by the row's `source_type`, falling back to an `IPAWSCAP`/`IPAWSWEA` marker in the raw header for older rows). Both are HTML-escaped. No backfill is needed -- these columns have been populated on every CAP insert all along, so existing archived alerts show their text immediately.

- **CAP instructions are now included in notifications.** Discord embeds get a "CAP Instructions:" field, and the AppRise markdown, HTML, and plaintext bodies each get a matching section. On Discord the field is only added when it actually fits: instructions longer than the 1024-character field limit, or that would push the embed past Discord's 6000-character total, are omitted entirely rather than truncated mid-sentence, and the omission is logged.

---

## v0.32.0: Released 2026-07-22

- **ARM images are here.** `latest` is now a multi-arch manifest covering `linux/amd64`, `linux/arm64`, and `linux/arm/v7`, so everything from a Raspberry Pi 2 on 32-bit Pi OS to a Pi 5 on a 64-bit OS can pull the image directly with no config change. CI now builds with Buildx + QEMU instead of a plain `docker build`. Thanks to @UrkiMimi who opened the ARM request issue (#6).

- **Speechify Tom now ALSO runs natively on every architecture we publish.** [Speechify](https://github.com/wagwan-piffting-blud/Speechify) native engine release 2026.07.22 adds native `x86_64`, `arm64`, and `armv7` Linux builds, all of which are wired in. Verified: all four published Linux binaries (legacy 32-bit `x86`, `x86_64`, `arm64`, `armv7`) synthesize the same test phrase to *byte-identical* WAV output -- one unique SHA-256 across the whole set. A Raspberry Pi 2 on 32-bit Pi OS produces the same audio, sample for sample, as an x86-64 server, so there is no voice drift between deployments.

- **i386 multiarch is gone from the image.** amd64 now uses the native `spfy-linux-x86_64` build instead of the 32-bit `spfy-linux-x86` one, so `dpkg --add-architecture i386` and `libc6:i386` are no longer installed at all. Architecture selection lives in a `SPFY_ASSET_SLUG_*` / `SPFY_ASSET_SHA256_*` pair per arch, where a non-empty checksum is what switches that architecture on -- so picking up a future architecture is a two-line change, and the i386 branch only fires if an arch is deliberately pointed back at the legacy 32-bit asset.

- **The `-lite` image is deprecated and will stop being published after v0.32.0.** There is now one unified image built from one Dockerfile. This was made possible by a package audit that removed a surprising amount of dead weight: Liquidsoap (every audio path has always used ffmpeg), p7zip-full, tmux, gnupg2, wget, git, and six PHP extensions that no shipped PHP file ever called (`php-mysql`, `php-curl`, `php-gd`, `php-mbstring`, `php-xml`, `php-zip`). With that gone, `latest` and `latest-lite` were within ~20 MB of each other -- the Piper voice model by itself is larger than the entire Speechify payload -- so maintaining two diverging Dockerfiles no longer bought anything. `Dockerfile.lite` and `docker_entrypoint_lite.sh` are deleted; the `-lite` tag is still published from the unified Dockerfile via `--build-arg VARIANT=lite` so existing pulls keep working during the deprecation window.

- **TTS engine is now resolved at startup against what the image actually contains.** Leave `TTS_ENGINE` unset and the entrypoint picks Speechify Tom where it exists and Piper everywhere else. Ask for an engine the image does not have and you get a clear warning in the logs plus an automatic fallback to Piper, instead of a failure on the first CAP alert that needs TTS. `espeak-ng` is now installed in every image too -- it was always a supported `TTS_ENGINE` value in the code but was never actually present in the full image.

- **Dashboard notices.** The dashboard now renders a banner when you are running the deprecated `-lite` image, and a second banner when your requested TTS engine was unavailable and got substituted. Both are driven by an `image_info.json` that the entrypoint writes at every boot, so they need no configuration.

- **Speechify Tom voice blobs (`tom.vin`, `tom8.vdb`, `tom.vcf`) are now fetched from a pinned commit over HTTPS and verified with SHA-256, replacing an unpinned shallow `git clone` of `main`.** This makes the build simpler and drops `git` from the runtime image entirely.

- **Fixed the OpenSSL runtime dependency, which was named `libssl3`.** On Debian trixie that package has no installation candidate on *any* architecture -- it was only resolving on amd64 through virtual-package indirection, and it fails outright on armhf, where Debian's 64-bit `time_t` transition is visible. The image now installs `libssl3t64` by name, which is the real package on amd64, arm64, and armhf alike.

---

## v0.31.0: Released 2026-07-22

- **Some small changes across the board, linting, comment removal, etc. to reduce the size of the codebase and improve readability.**

---

## v0.30.0: Released 2026-07-13

- **Happy version 30!** I am introducing a new CHANGES.md file to keep track of all the changes made in EAS_Listener. This will help maintain a clear history of updates and improvements made to EAS_Listener. EAS Tools uses the same kind of CHANGES.md file to keep track of changes made in EAS Tools. The CHANGES.md file will be updated with each new version, and it will include a summary of the changes made, along with the version number and date of the release.

- **Introduce AGENTS.md file to help agentic development of EAS_Listener.** This file will contain information about the project to help coding agents understand the project and its goals. It will include details about the architecture, design patterns, and coding standards used in EAS_Listener. The AGENTS.md file will be updated as needed to provide the most up-to-date information for coding agents working on EAS_Listener.

- **Reduce Speechify Tom TTS dependency size to only the bare minimum required for the TTS engine to function.** This will help reduce the overall size of the project and improve performance. (We also swapped over from Wine to spfy_synth, a NATIVE Linux binary that will run on Linux without Wine, which is a huge improvement for performance and stability. The UID match, however, is still 100% with the real Speechify Tom thanks to the in-line DLL FE loader.)

- **Complete Icecast 2 stream integration.** This was pending for the longest time, but never finished. Currently, this means that the listener can now output its own stream of alerts only, 24/7. There is a normal alert queue for frequent alert periods. The Icecast RELAY portion is 100% unmodified and works just the same.

- **Add "Send Test Alert" button to the EAS_Listener GUI.** This allows users to easily test the whole alert system pipeline without having to wait for an actual alert to occur. The test alert will simulate a real alert and will be sent through the same channels as a real alert, allowing users to verify that their setup is working correctly. Helpful if you recently changed something and don't know if your changes will work. The test alert will also be logged in the alert history for reference. Major thanks to GitHub user [@averlice](https://github.com/averlice) for the idea.

- **Remove errant "icecast.xml" file from the Dockerfile.** This file is not tracked locally and was causing build issues. Icecast should supply its own file.
