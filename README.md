# Rust EAS Listener and Notifier

*A software ENDEC that listens to broadcast audio streams, decodes EAS/SAME messages, records audio, and pushes rich notifications via Apprise with a real-time monitoring dashboard. Also supports Make Your Own DASDEC, relaying to Icecast, and event/FIPS-code based filtering.*

---

![Dashboard image](./dashboard.png)

---

## Features

- Real-time EAS/SAME message decoding from multiple audio sources (primarily Icecast/Shoutcast streams)
- Includes 1050Hz tone detection for NWR streams that are not SAME-toned
- Audio recording and optional Icecast relaying
- Rich notifications via [Apprise](https://github.com/caronc/apprise) and Discord embed support
- Web-based monitoring dashboard
- [Make Your Own DASDEC](https://github.com/playsamay4/MYOD) support
- Event-code based filtering
- Docker image with everything pre-configured and included
- Highly configurable via JSON
- Modular and extensible architecture
- Written in Rust for ultimate performance and memory safety

---

## Installation, configuration, usage, technical details

[Please refer to the wiki](https://github.com/wagwan-piffting-blud/EAS_Listener/wiki) for detailed instructions on installation, configuration, usage, and more that this README cannot cover in-depth.

---

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The dashboard will check for updates on GitHub and notify users when a new version is available. Patch versions are for bug fixes and minor improvements to the frontend, minor versions are for new features or bug fixes to the backend, and major versions are mostly unused (per Rust ecosystem tradition). Please refer to the project's commit history for detailed changes and updates.

---

## License

This project is licensed under the **GNU GPL-3.0** (see [`LICENSE`](LICENSE)).

---

## Acknowledgments
- [\@\_spchalethorpe09\_](https://sterlingvaspc.neocities.org/) on Discord for thorough testing, feedback, and suggestions
- Global Weather and EAS Society (GWES) for their overall support and resources
- SAME decoders and EAS/NWR community research
- Rust ecosystem maintainers

## GenAI Disclosure Notice: Portions of this repository have been generated using Generative AI tools (ChatGPT, ChatGPT Codex, GitHub Copilot).
