This is the codebase for a project called EAS_Listener. This codebase is a Rust project designed to listen for and relay Emergency Alert System (EAS) messages. The project is structured to handle incoming EAS messages, process them, and then relay the information to designated endpoints or systems using Apprise, Icecast, or MYOD (Make Your Own DASDEC; see https://github.com/wagwan-piffting-blud/MYOD/tree/cross-platform-with-audio).

When working with the codebase, it is important to understand the following major components of the project:

**Two Dockerfiles**: The project includes two Dockerfiles, one for the "full" version of the project, which includes all dependencies and features, and one for a "minimal" version, which is designed to be more lightweight. The full version is intended for users who want to use Speechify Tom as the CAP alert TTS voice (this requires the full version), while the minimal version is for users who want a smaller footprint or have specific requirements (this version uses the Piper TTS engine instead).

**EAS Message Handling**: The core functionality of the project revolves around receiving EAS messages, which are typically broadcast over Icecast streams (which the user adds via config.json, the project supports "infinite sources", meaning there is no hard limit, only resource constraints). The codebase includes modules for parsing these messages and extracting relevant information. One core module is E2T-NG, which is responsible for converting EAS messages into a format that can be easily understood by humans. See https://github.com/wagwan-piffting-blud/E2T-NG for more details.

**Relaying Mechanisms**: After processing the EAS messages, the project can relay the information to various endpoints. The codebase includes support for Apprise, which allows for sending notifications to multiple platforms (like email, SMS, and messaging apps). Additionally, it supports Icecast for streaming audio alerts and MYOD for custom DASDEC-type implementations.

**Configuration**: The project uses a configuration file (config.json) to manage settings such as the list of Icecast sources, Apprise endpoints, and other parameters. Users can customize this file to suit their specific needs. The configuration file is crucial for the proper functioning of the listener, as it dictates how messages are received and where they are sent. The full configuration options, including examples, can be found at the GitHub wiki page at https://github.com/wagwan-piffting-blud/EAS_Listener/wiki/Configuration.

**Logging and Monitoring**: The codebase includes logging functionality to track the processing of EAS messages and any errors that may occur. This is important for debugging and ensuring that the listener operates correctly. Users can configure the logging level in the configuration file.

**Web Server**: The project includes a web server component that provides a user interface for monitoring the status of the listener, viewing logs, and managing configurations. This web interface can be accessed through a browser and provides real-time updates on the system's operation. Note that the web server and the Rust backend are separate components, and the web server is provided to interface with the core functionality of the listener.

**Semantic Versioning**: The project loosely follows semantic versioning principles, with version numbers indicating the level of changes made. Patch versions are for bug fixes and minor improvements to the frontend (web server), minor versions are for new features or bug fixes to the backend (Rust code), and major versions are mostly unused (per Rust ecosystem tradition).

**Releases**: When preparing a new release, run the following commands, in order, to ensure that the release is properly prepared for me to commit and push to GitHub (I handle all final commits). These commands will format the code, check for errors, run tests, and build the release version of the project:

```bash
cargo fmt
cargo check
cargo test
cargo build --release
```

If **ANY** cargo warnings or errors appear, the release should not be made until they are resolved. This project operates under a zero warnings policy, and any warnings or errors must be addressed before a release can be made. Note that releases are automatically built and pushed to GitHub Container Registry (GHCR) for Docker users (the only supported platform), and the release process is automated to ensure that the latest version is always available for users.
